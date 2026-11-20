//! Checks of shared-credential profile routes (spec: messaging-gateway,
//! Shared-credential profile routes): each route names its platform and a
//! served profile other than this one, matches on non-empty ids only (an
//! empty `user_id` would silently widen the route), and its target must
//! accept routes from this profile in its own `gateway.accept_routes_from`.

use std::path::Path;

use pmt_core::error::{Error, codes};

use crate::schema::Config;
use crate::write::ConfigFile;

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

/// The `gateway.accept_routes_from` of the profile in `dir` (empty when
/// its config does not set it or does not parse: its own load reports
/// that); `None` when there is no such profile. The configuration is the
/// profile's own: its remote plane document when the home uses one.
///
/// # Errors
///
/// An unreachable plane (never answered from the local file instead).
fn accepted_by(dir: &Path) -> Result<Option<Vec<String>>, Error> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let text = ConfigFile::of_profile_dir(dir)
        .raw()
        .or_else(|e| {
            if e.code() == &codes::HOME_IO {
                Ok(None)
            } else {
                Err(e)
            }
        })?
        .unwrap_or_default();
    let value: toml::Value =
        toml::from_str(&text).unwrap_or(toml::Value::Table(toml::Table::new()));
    Ok(Some(
        value
            .get("gateway")
            .and_then(|g| g.get("accept_routes_from"))
            .and_then(toml::Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// Refuses malformed routes and routes their target did not accept.
pub(crate) fn check(config: &Config, profile_dir: &Path) -> Result<(), Error> {
    let me = profile_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (index, route) in config.gateway.profile_routes.iter().enumerate() {
        let key = format!("gateway.profile_routes[{index}]");
        if route.name.trim().is_empty() || route.platform.trim().is_empty() {
            return Err(invalid(format!("{key}: name and platform are required")));
        }
        let ids = [
            ("guild_id", &route.guild_id),
            ("chat_id", &route.chat_id),
            ("thread_id", &route.thread_id),
            ("user_id", &route.user_id),
        ];
        if let Some((field, _)) = ids
            .iter()
            .find(|(_, id)| id.as_deref().is_some_and(|i| i.trim().is_empty()))
        {
            return Err(invalid(format!(
                "{key}.{field}: an empty id would match more than meant; give the id or leave the key out"
            )));
        }
        if route.profile == me {
            return Err(invalid(format!(
                "{key}.profile: '{me}' is this profile; a route sends events to another one"
            )));
        }
        let Some(parent) = profile_dir.parent() else {
            continue;
        };
        match accepted_by(&parent.join(&route.profile))? {
            None => {
                return Err(invalid(format!(
                    "{key}.profile: no profile named '{}'",
                    route.profile
                )));
            }
            Some(accepted) if !accepted.contains(&me) => {
                return Err(invalid(format!(
                    "{key}: profile '{}' does not accept routes from '{me}'; add \"{me}\" to its gateway.accept_routes_from",
                    route.profile
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles(target_config: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let profiles = home.path().join("profiles");
        let me = profiles.join("default");
        std::fs::create_dir_all(&me).unwrap();
        let ops = profiles.join("ops");
        std::fs::create_dir_all(&ops).unwrap();
        std::fs::write(ops.join("config.toml"), target_config).unwrap();
        (home, me)
    }

    fn config(route: &str) -> Config {
        toml::from_str(&format!("[[gateway.profile_routes]]\n{route}")).unwrap()
    }

    /// Spec scenario: Target did not opt in.
    #[test]
    fn a_target_that_did_not_opt_in_fails_naming_it() {
        let (_home, me) = profiles("");
        let routed =
            config("name = \"ops\"\nplatform = \"discord\"\nguild_id = \"1\"\nprofile = \"ops\"\n");
        let err = check(&routed, &me).unwrap_err();
        assert!(
            err.message().contains("'ops'") && err.message().contains("gateway.accept_routes_from"),
            "{}",
            err.message()
        );
        let (_home, me) = profiles("[gateway]\naccept_routes_from = [\"default\"]\n");
        check(&routed, &me).unwrap();
    }

    #[test]
    fn an_empty_user_id_is_refused_rather_than_widening() {
        let (_home, me) = profiles("[gateway]\naccept_routes_from = [\"default\"]\n");
        let routed =
            config("name = \"x\"\nplatform = \"discord\"\nuser_id = \"\"\nprofile = \"ops\"\n");
        let err = check(&routed, &me).unwrap_err();
        assert!(err.message().contains("user_id"), "{}", err.message());
        let unknown = config("name = \"x\"\nplatform = \"discord\"\nprofile = \"nobody\"\n");
        assert!(
            check(&unknown, &me)
                .unwrap_err()
                .message()
                .contains("no profile named 'nobody'")
        );
    }

    /// Spec: configuration-and-profiles, Remote configuration backend: a
    /// remote profile's opt-in is the plane's; its local `config.toml` (a
    /// seeded template) is never read in its place.
    #[test]
    fn a_remote_targets_opt_in_comes_from_the_plane() {
        let routed =
            config("name = \"ops\"\nplatform = \"discord\"\nguild_id = \"1\"\nprofile = \"ops\"\n");
        let home = tempfile::tempdir().unwrap();
        crate::remote::fake::serve(
            home.path(),
            &[
                ("default", serde_json::json!({})),
                (
                    "ops",
                    serde_json::json!({"gateway": {"accept_routes_from": ["default"]}}),
                ),
            ],
        );
        let me = home.path().join("profiles/default");
        check(&routed, &me).unwrap();

        let home = tempfile::tempdir().unwrap();
        crate::remote::fake::serve(
            home.path(),
            &[
                ("default", serde_json::json!({})),
                ("ops", serde_json::json!({})),
            ],
        );
        std::fs::write(
            home.path().join("profiles/ops/config.toml"),
            "[gateway]\naccept_routes_from = [\"default\"]\n",
        )
        .unwrap();
        let me = home.path().join("profiles/default");
        let err = check(&routed, &me).unwrap_err();
        assert!(
            err.message().contains("does not accept routes"),
            "{}",
            err.message()
        );
    }
}
