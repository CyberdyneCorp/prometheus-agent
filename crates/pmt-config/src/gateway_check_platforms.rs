//! Whole-table checks of `[gateway.platforms.<name>]` that need more than
//! the options schema (spec: platform-adapters, First-party adapter
//! transports and required settings; Adapter proxy settings and
//! environment trust; Telegram webhook mode; Email adapter behaviour;
//! Webhook routes; Webhook payload filters and transform scripts; Webhook
//! direct delivery mode): settings an enabled adapter cannot start
//! without, proxy URLs, and webhook routes.

use std::path::{Component, Path};

use pmt_core::error::{Error, codes};

use crate::platforms::{
    self, EmailOptions, MatrixOptions, SignalOptions, TelegramMode, TelegramOptions,
};
use crate::schema::{Config, PlatformConfig, WebhookFilter, WebhookFilters, WebhookRoute};

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

/// Every platform table's cross-key rules, then the shared proxy.
pub(crate) fn check(config: &Config) -> Result<(), Error> {
    for (platform, table) in &config.gateway.platforms {
        let key = format!("gateway.platforms.{platform}");
        check_proxy(&format!("{key}.proxy_url"), table.proxy_url.as_deref())?;
        if table.max_body_bytes.is_some() && platform != "webhook" {
            return Err(invalid(format!(
                "{key}.max_body_bytes: only the webhook platform reads request bodies"
            )));
        }
        check_routes(platform, table)?;
        check_mode_rules(platform, &key, table)?;
        if table.enabled {
            check_required(platform, &key, table)?;
        }
    }
    check_proxy("gateway.proxy_url", config.gateway.proxy_url.as_deref())
}

/// A proxy URL uses a supported scheme and carries no password (a URL
/// with credentials belongs in the secrets store).
fn check_proxy(key: &str, url: Option<&str>) -> Result<(), Error> {
    let Some(url) = url else { return Ok(()) };
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(invalid(format!(
            "{key}: '{url}' is not a URL; use http://, https:// or socks5://"
        )));
    };
    if !matches!(scheme, "http" | "https" | "socks5") {
        return Err(invalid(format!(
            "{key}: unsupported proxy scheme '{scheme}'; use http, https or socks5"
        )));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let password = authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'));
    if password {
        return Err(invalid(format!(
            "{key}: the URL carries a password; store it with `prometheus secrets set <NAME>` and name the secret in {key}_secret"
        )));
    }
    Ok(())
}

fn options<T: serde::de::DeserializeOwned + Default>(platform: &str, table: &PlatformConfig) -> T {
    // The schema check already refused a table that does not fit.
    platforms::options(platform, &table.options).unwrap_or_default()
}

fn missing(key: &str, option: &str, why: &str) -> Error {
    invalid(format!("{key}.options.{option}: required {why}"))
}

/// Rules that hold whether or not the adapter is enabled.
fn check_mode_rules(platform: &str, key: &str, table: &PlatformConfig) -> Result<(), Error> {
    if platform == "telegram" {
        let telegram: TelegramOptions = options(platform, table);
        if telegram.mode == TelegramMode::Webhook {
            if telegram.webhook_secret_secret.is_none() {
                return Err(invalid(format!(
                    "{key}.options.webhook_secret_secret: webhook mode requires a secret token (the secret Telegram sends with every update)"
                )));
            }
            if telegram
                .webhook_public_url
                .as_deref()
                .is_none_or(|u| u.trim().is_empty())
            {
                return Err(missing(
                    key,
                    "webhook_public_url",
                    "in webhook mode (the public URL Telegram posts updates to)",
                ));
            }
        }
    }
    if platform == "email"
        && let Some(entry) = table.allow_users.iter().find(|u| !whole_address(u))
    {
        return Err(invalid(format!(
            "{key}.allow_users: '{entry}' is not a whole address; email allowlists name addresses like user@example.org"
        )));
    }
    Ok(())
}

fn whole_address(entry: &str) -> bool {
    entry
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
}

/// Settings an enabled adapter cannot start without.
fn check_required(platform: &str, key: &str, table: &PlatformConfig) -> Result<(), Error> {
    match platform {
        "email" => {
            let email: EmailOptions = options(platform, table);
            for (option, value) in [
                ("address", &email.address),
                ("imap_host", &email.imap_host),
                ("smtp_host", &email.smtp_host),
            ] {
                if value.as_deref().is_none_or(|v| v.trim().is_empty()) {
                    return Err(missing(key, option, "when email is enabled"));
                }
            }
            if email.require_authenticated_sender && email.authserv_id.is_none() {
                return Err(missing(
                    key,
                    "authserv_id",
                    "to authenticate senders (the server whose Authentication-Results header is trusted), unless require_authenticated_sender = false",
                ));
            }
        }
        "matrix" => {
            let matrix: MatrixOptions = options(platform, table);
            if matrix.homeserver.is_none() {
                return Err(missing(key, "homeserver", "when matrix is enabled"));
            }
        }
        "signal" => {
            let signal: SignalOptions = options(platform, table);
            if signal.http_url.is_none() {
                return Err(missing(
                    key,
                    "http_url",
                    "when signal is enabled (the signal-cli HTTP daemon)",
                ));
            }
            match signal.account.as_deref() {
                None => return Err(missing(key, "account", "when signal is enabled")),
                Some(account) if !e164(account) => {
                    return Err(invalid(format!(
                        "{key}.options.account: '{account}' is not an E.164 number (+<country><number>)"
                    )));
                }
                Some(_) => {}
            }
        }
        _ => {}
    }
    Ok(())
}

fn e164(number: &str) -> bool {
    number.strip_prefix('+').is_some_and(|digits| {
        (7..=15).contains(&digits.len())
            && digits.chars().all(|c| c.is_ascii_digit())
            && !digits.starts_with('0')
    })
}

/// Routes belong to the webhook platform; each needs a signing secret
/// unless it explicitly admits unsigned requests.
fn check_routes(platform: &str, table: &PlatformConfig) -> Result<(), Error> {
    for (name, route) in &table.routes {
        let key = format!("gateway.platforms.{platform}.routes.{name}");
        if platform != "webhook" {
            return Err(invalid(format!(
                "{key}: only the webhook platform has routes"
            )));
        }
        check_route_name(&key, name)?;
        if route.secret_secret.is_none() && !route.insecure_no_auth {
            return Err(invalid(format!(
                "{key}.secret_secret: required unless insecure_no_auth = true"
            )));
        }
        check_route_delivery(&key, route)?;
        if let Some(script) = &route.script {
            check_confined(
                &format!("{key}.script"),
                script,
                "the profile's scripts directory",
            )?;
        }
        for condition in route.filters.iter().flat_map(WebhookFilters::conditions) {
            check_filter(&format!("{key}.filters"), condition)?;
        }
    }
    Ok(())
}

fn check_route_name(key: &str, name: &str) -> Result<(), Error> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "{key}: a route name uses only a-z, 0-9, '-' and '_'"
        )))
    }
}

fn check_route_delivery(key: &str, route: &WebhookRoute) -> Result<(), Error> {
    let target = route.deliver_target();
    if route.deliver_only && target == "log" {
        return Err(invalid(format!(
            "{key}.deliver: deliver_only sends the rendered prompt somewhere; set deliver to a send target (platform[:chat_id[:thread_id]])"
        )));
    }
    if route.deliver_only && matches!(target, "origin" | "github_comment") {
        return Err(invalid(format!(
            "{key}.deliver: deliver_only needs a send target (platform[:chat_id[:thread_id]]), not '{target}'"
        )));
    }
    let extra = route
        .deliver_extra
        .iter()
        .map(|t| ("deliver_extra", t.as_str()));
    for (field, target) in std::iter::once(("deliver", target)).chain(extra) {
        let builtin = field == "deliver" && matches!(target, "log" | "origin" | "github_comment");
        let platform = target.split(':').next().unwrap_or_default();
        if !builtin && !platforms::is_known(platform) {
            return Err(invalid(format!(
                "{key}.{field}: '{target}' is not a send target (platform[:chat_id[:thread_id]])"
            )));
        }
    }
    Ok(())
}

/// A relative path that stays inside its directory.
fn check_confined(key: &str, path: &str, within: &str) -> Result<(), Error> {
    let confined = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if confined {
        Ok(())
    } else {
        Err(invalid(format!(
            "{key}: '{path}' leaves {within} (path traversal); give a relative path inside it"
        )))
    }
}

fn check_filter(key: &str, filter: &WebhookFilter) -> Result<(), Error> {
    let mut found = Ok(());
    filter.visit(&mut |condition| {
        if found.is_err() {
            return;
        }
        if let Some(path) = condition.path()
            && !(path == "event" || path.starts_with("payload.") || path.starts_with("headers."))
        {
            found = Err(invalid(format!(
                "{key}: path '{path}' must be event, payload.<path> or headers.<Name>"
            )));
            return;
        }
        found = match condition {
            WebhookFilter::Regex(p) => regex::Regex::new(&p.pattern)
                .map(drop)
                .map_err(|e| invalid(format!("{key}: invalid regex '{}': {e}", p.pattern))),
            WebhookFilter::InFile(f) => {
                check_confined(&format!("{key}.in_file"), &f.file, "the profile directory")
            }
            _ => Ok(()),
        };
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_toml(text: &str) -> Result<(), Error> {
        let config: Config = toml::from_str(text).unwrap();
        check(&config)
    }

    fn message(text: &str) -> String {
        check_toml(text).unwrap_err().message().to_owned()
    }

    #[test]
    fn email_missing_host_names_the_option() {
        let found = message(
            "[gateway.platforms.email]\nenabled = true\noptions = { address = \"a@x.org\", smtp_host = \"smtp.x.org\", authserv_id = \"mx.x.org\" }\n",
        );
        assert!(
            found.starts_with("gateway.platforms.email.options.imap_host"),
            "{found}"
        );
        // A disabled adapter is not checked.
        check_toml("[gateway.platforms.email]\noptions = { address = \"a@x.org\" }\n").unwrap();
    }

    #[test]
    fn email_needs_a_trusted_authentication_server_and_whole_addresses() {
        let base = "[gateway.platforms.email]\nenabled = true\noptions = { address = \"a@x.org\", imap_host = \"i\", smtp_host = \"s\" ";
        assert!(message(&format!("{base}}}\n")).contains("options.authserv_id"));
        check_toml(&format!(
            "{base}, require_authenticated_sender = false }}\n"
        ))
        .unwrap();
        check_toml(&format!("{base}, authserv_id = \"mx.google.com\" }}\n")).unwrap();
        let domain = message("[gateway.platforms.email]\nallow_users = [\"example.org\"]\n");
        assert!(domain.contains("not a whole address"), "{domain}");
    }

    #[test]
    fn signal_and_matrix_required_settings() {
        assert!(
            message("[gateway.platforms.matrix]\nenabled = true\n").contains("options.homeserver")
        );
        assert!(
            message("[gateway.platforms.signal]\nenabled = true\noptions = { account = \"+15551234567\" }\n")
                .contains("options.http_url")
        );
        assert!(
            message("[gateway.platforms.signal]\nenabled = true\noptions = { http_url = \"http://127.0.0.1:8080\", account = \"5551234\" }\n")
                .contains("E.164")
        );
        check_toml("[gateway.platforms.signal]\nenabled = true\noptions = { http_url = \"http://127.0.0.1:8080\", account = \"+15551234567\" }\n").unwrap();
    }

    #[test]
    fn proxy_urls_use_known_schemes_and_keep_passwords_secret() {
        assert!(
            message("[gateway]\nproxy_url = \"ftp://p:1\"\n").contains("unsupported proxy scheme")
        );
        let password = message("[gateway.platforms.discord]\nproxy_url = \"http://u:pw@p:3128\"\n");
        assert!(password.contains("proxy_url_secret"), "{password}");
        check_toml("[gateway]\nproxy_url = \"socks5://127.0.0.1:1080\"\n").unwrap();
        check_toml("[gateway.platforms.slack]\nproxy_url = \"https://user@proxy:443\"\n").unwrap();
    }

    #[test]
    fn telegram_webhook_mode_requires_a_secret_token() {
        let found = message(
            "[gateway.platforms.telegram]\noptions = { mode = \"webhook\", webhook_public_url = \"https://bot.example/p/default/telegram/\" }\n",
        );
        assert!(
            found.contains("webhook mode requires a secret token"),
            "{found}"
        );
        check_toml("[gateway.platforms.telegram]\noptions = { mode = \"webhook\", webhook_public_url = \"https://bot.example/p/default/telegram/\", webhook_secret_secret = \"TG_HOOK\" }\n").unwrap();
    }

    #[test]
    fn webhook_routes_need_a_secret_and_belong_to_the_webhook_platform() {
        let unsigned = message("[gateway.platforms.webhook.routes.ci]\nevents = [\"push\"]\n");
        assert!(
            unsigned.contains("gateway.platforms.webhook.routes.ci.secret_secret"),
            "{unsigned}"
        );
        let elsewhere = message("[gateway.platforms.slack.routes.ci]\nsecret_secret = \"S\"\n");
        assert!(elsewhere.contains("only the webhook platform"));
        let bad_name =
            message("[gateway.platforms.webhook.routes.\"C I\"]\nsecret_secret = \"S\"\n");
        assert!(bad_name.contains("a route name uses only"));
        check_toml("[gateway.platforms.webhook.routes.ci]\nsecret_secret = \"S\"\n").unwrap();
        check_toml("[gateway.platforms.webhook.routes.local]\ninsecure_no_auth = true\n").unwrap();
    }

    #[test]
    fn a_script_outside_the_scripts_directory_is_path_traversal() {
        let found = message(
            "[gateway.platforms.webhook.routes.ci]\nsecret_secret = \"S\"\nscript = \"../../bin/sh\"\n",
        );
        assert!(
            found.contains("routes.ci.script") && found.contains("path traversal"),
            "{found}"
        );
        assert!(
            message("[gateway.platforms.webhook.routes.ci]\nsecret_secret = \"S\"\nscript = \"/bin/sh\"\n")
                .contains("path traversal")
        );
        check_toml("[gateway.platforms.webhook.routes.ci]\nsecret_secret = \"S\"\nscript = \"ci/transform.py\"\n").unwrap();
    }

    #[test]
    fn deliver_only_needs_a_send_target() {
        let found = message(
            "[gateway.platforms.webhook.routes.alert]\nsecret_secret = \"S\"\ndeliver_only = true\n",
        );
        assert!(found.contains("routes.alert.deliver"), "{found}");
        check_toml("[gateway.platforms.webhook.routes.alert]\nsecret_secret = \"S\"\ndeliver_only = true\ndeliver = \"telegram\"\n").unwrap();
        assert!(
            message("[gateway.platforms.webhook.routes.a]\nsecret_secret = \"S\"\ndeliver = \"myspace:1\"\n")
                .contains("not a send target")
        );
    }

    #[test]
    fn filters_name_valid_paths_and_patterns() {
        let route = "[gateway.platforms.webhook.routes.ci]\nsecret_secret = \"S\"\n";
        check_toml(&format!(
            "{route}filters = {{ equals = {{ path = \"payload.workflow_run.conclusion\", value = \"failure\" }} }}\n"
        ))
        .unwrap();
        check_toml(&format!(
            "{route}filters = [{{ any = [{{ exists = {{ path = \"headers.X-Trace\" }} }}, {{ not = {{ in = {{ path = \"event\", values = [\"push\"] }} }} }}] }}]\n"
        ))
        .unwrap();
        assert!(
            message(&format!(
                "{route}filters = {{ regex = {{ path = \"event\", pattern = \"(\" }} }}\n"
            ))
            .contains("invalid regex")
        );
        assert!(
            message(&format!(
                "{route}filters = {{ exists = {{ path = \"body.x\" }} }}\n"
            ))
            .contains("must be event, payload.<path> or headers.<Name>")
        );
    }
}
