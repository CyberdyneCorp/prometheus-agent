//! The proxies the environment names, for `gateway.trust_env = true`
//! (spec: platform-adapters, Adapter proxy settings and environment
//! trust): the proxy environment variables first, then the operating
//! system's proxy settings for whatever the variables leave unset (the
//! order curl, reqwest and Python's `urllib` use). The OS settings are
//! read only here, for adapters under environment trust; every other HTTP
//! client of the binary keeps its own rules.
//!
//! - macOS: the network configuration (`scutil --proxy`): HTTP, HTTPS and
//!   SOCKS proxies with their exceptions list;
//! - Windows: the user's Internet Settings (`ProxyEnable`, `ProxyServer`,
//!   `ProxyOverride`);
//! - elsewhere the environment variables are the system settings.

/// Proxies by destination scheme, and the hosts that bypass them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxySettings {
    /// For `http://` destinations.
    pub http: Option<String>,
    /// For `https://` (and `wss://`) destinations.
    pub https: Option<String>,
    /// For any destination the scheme-specific entries leave unset.
    pub all: Option<String>,
    /// Hosts, domains (`.example.com`) and networks reached directly.
    pub bypass: Vec<String>,
}

impl ProxySettings {
    /// The environment's settings: its variables, then the OS settings.
    #[must_use]
    pub fn trusted() -> Self {
        Self::from_vars(|name| std::env::var(name).ok()).or(system())
    }

    /// The settings `var` holds (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
    /// `NO_PROXY`, upper case first).
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let first = |names: [&str; 2]| {
            names
                .into_iter()
                .find_map(|name| var(name).filter(|v| !v.trim().is_empty()))
        };
        Self {
            http: first(["HTTP_PROXY", "http_proxy"]),
            https: first(["HTTPS_PROXY", "https_proxy"]),
            all: first(["ALL_PROXY", "all_proxy"]),
            bypass: first(["NO_PROXY", "no_proxy"])
                .map(|list| split_list(&list, ','))
                .unwrap_or_default(),
        }
    }

    /// These settings, falling back to `other` for whatever is unset.
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        Self {
            http: self.http.or(other.http),
            https: self.https.or(other.https),
            all: self.all.or(other.all),
            bypass: if self.bypass.is_empty() {
                other.bypass
            } else {
                self.bypass
            },
        }
    }

    /// No proxy at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.http.is_none() && self.https.is_none() && self.all.is_none()
    }

    /// The proxy URL for a `tls` (or plain) destination.
    #[must_use]
    pub fn proxy_for(&self, tls: bool) -> Option<String> {
        let own = if tls { &self.https } else { &self.http };
        own.as_ref()
            .or(self.all.as_ref())
            .map(|url| with_scheme(url))
    }

    /// Whether `host` is reached directly.
    #[must_use]
    pub fn bypasses(&self, host: &str) -> bool {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        self.bypass.iter().any(|entry| bypassed(entry, &host))
    }

    /// The bypass list as reqwest's `NO_PROXY` syntax.
    #[must_use]
    pub fn no_proxy(&self) -> String {
        self.bypass.join(",")
    }

    /// The settings `scutil --proxy` prints (macOS).
    #[must_use]
    pub fn from_scutil(text: &str) -> Self {
        let mut keys = std::collections::HashMap::new();
        let mut bypass = Vec::new();
        let mut in_exceptions = false;
        for line in text.lines().map(str::trim) {
            if in_exceptions {
                match line.split_once(" : ") {
                    Some((_, host)) => bypass.push(normalize_bypass(host)),
                    None => in_exceptions = false,
                }
                continue;
            }
            match line.split_once(" : ") {
                Some(("ExceptionsList", _)) => in_exceptions = true,
                Some((key, value)) => {
                    keys.insert(key.to_owned(), value.to_owned());
                }
                None => {}
            }
        }
        let proxy = |kind: &str, scheme: &str| -> Option<String> {
            if keys.get(&format!("{kind}Enable")).map(String::as_str) != Some("1") {
                return None;
            }
            let host = keys.get(&format!("{kind}Proxy"))?;
            Some(match keys.get(&format!("{kind}Port")) {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            })
        };
        Self {
            http: proxy("HTTP", "http"),
            https: proxy("HTTPS", "http"),
            all: proxy("SOCKS", "socks5"),
            bypass,
        }
    }

    /// The settings `reg query` prints for the user's Internet Settings
    /// (Windows): `ProxyServer` is one `host:port` for every scheme or
    /// `http=host:port;https=host:port;socks=host:port`.
    #[must_use]
    pub fn from_reg_query(text: &str) -> Self {
        let value = |name: &str| {
            text.lines().find_map(|line| {
                let mut parts = line.split_whitespace();
                (parts.next() == Some(name))
                    .then(|| parts.nth(1).map(str::to_owned))
                    .flatten()
            })
        };
        let enabled = value("ProxyEnable")
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .is_some_and(|v| v != 0);
        let Some(server) = value("ProxyServer").filter(|_| enabled) else {
            return Self::default();
        };
        let bypass = value("ProxyOverride")
            .map(|list| split_list(&list, ';'))
            .unwrap_or_default();
        if !server.contains('=') {
            let url = with_scheme(&server);
            return Self {
                http: Some(url.clone()),
                https: Some(url),
                all: None,
                bypass,
            };
        }
        let mut settings = Self {
            bypass,
            ..Self::default()
        };
        for entry in server.split(';') {
            match entry.split_once('=') {
                Some(("http", url)) => settings.http = Some(with_scheme(url)),
                Some(("https", url)) => settings.https = Some(with_scheme(url)),
                Some(("socks", url)) => settings.all = Some(format!("socks5://{url}")),
                _ => {}
            }
        }
        settings
    }
}

/// `host:port` as a URL (proxy settings often omit the scheme).
fn with_scheme(url: &str) -> String {
    if url.contains("://") {
        url.to_owned()
    } else {
        format!("http://{url}")
    }
}

fn split_list(list: &str, separator: char) -> Vec<String> {
    list.split(separator)
        .map(str::trim)
        // `<local>` (Windows: names without a dot) has no reqwest form.
        .filter(|entry| !entry.is_empty() && *entry != "<local>")
        .map(normalize_bypass)
        .collect()
}

/// `*.local` -> `.local`; `169.254/16` and `169.254.*` ->
/// `169.254.0.0/16`.
fn normalize_bypass(entry: &str) -> String {
    let entry = entry.trim().to_ascii_lowercase();
    if let Some(domain) = entry.strip_prefix("*.") {
        return format!(".{domain}");
    }
    if let Some(net) = entry.strip_suffix(".*") {
        let net = net.trim_end_matches(".*");
        let octets = net.split('.').count();
        if octets < 4 && net.split('.').all(|o| o.parse::<u8>().is_ok()) {
            return format!("{net}{}/{}", ".0".repeat(4 - octets), octets * 8);
        }
    }
    if let Some((net, bits)) = entry.split_once('/') {
        let octets = net.split('.').count();
        if octets < 4 && net.split('.').all(|o| o.parse::<u8>().is_ok()) {
            return format!("{net}{}/{bits}", ".0".repeat(4 - octets));
        }
    }
    entry
}

fn bypassed(entry: &str, host: &str) -> bool {
    if entry == "*" {
        return true;
    }
    if let Some((net, bits)) = entry.split_once('/') {
        return in_network(host, net, bits);
    }
    let domain = entry.trim_start_matches('.');
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn in_network(host: &str, net: &str, bits: &str) -> bool {
    let (Ok(host), Ok(net), Ok(bits)) = (
        host.parse::<std::net::Ipv4Addr>(),
        net.parse::<std::net::Ipv4Addr>(),
        bits.parse::<u32>(),
    ) else {
        return false;
    };
    let mask = u32::MAX.checked_shl(32 - bits.min(32)).unwrap_or(0);
    u32::from(host) & mask == u32::from(net) & mask
}

/// The operating system's proxy settings (empty where the environment
/// variables are the system settings, or when they cannot be read).
#[must_use]
pub fn system() -> ProxySettings {
    #[cfg(target_os = "macos")]
    {
        command_output(std::process::Command::new("/usr/sbin/scutil").arg("--proxy"))
            .map(|text| ProxySettings::from_scutil(&text))
            .unwrap_or_default()
    }
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let reg = std::path::Path::new(&root).join("System32").join("reg.exe");
        let mut command = std::process::Command::new(reg);
        command.args([
            "query",
            "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings",
        ]);
        // No console window flashes from a background daemon.
        std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
        command_output(&mut command)
            .map(|text| ProxySettings::from_reg_query(&text))
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        ProxySettings::default()
    }
}

#[cfg(any(target_os = "macos", windows))]
fn command_output(command: &mut std::process::Command) -> Option<String> {
    let output = command
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCUTIL: &str = "<dictionary> {
  ExceptionsList : <array> {
    0 : *.local
    1 : 169.254/16
  }
  FTPPassive : 1
  HTTPEnable : 1
  HTTPPort : 3128
  HTTPProxy : proxy.corp.example
  HTTPSEnable : 1
  HTTPSPort : 3129
  HTTPSProxy : proxy.corp.example
  SOCKSEnable : 0
  SOCKSPort : 1080
  SOCKSProxy : socks.corp.example
}
";

    #[test]
    fn macos_settings_give_each_enabled_proxy_and_the_exceptions() {
        let settings = ProxySettings::from_scutil(SCUTIL);
        assert_eq!(
            settings.http.as_deref(),
            Some("http://proxy.corp.example:3128")
        );
        assert_eq!(
            settings.proxy_for(true).as_deref(),
            Some("http://proxy.corp.example:3129")
        );
        assert_eq!(settings.all, None, "SOCKS is disabled");
        assert!(settings.bypasses("printer.local"));
        assert!(settings.bypasses("169.254.10.1"));
        assert!(!settings.bypasses("api.telegram.org"));
        let none = ProxySettings::from_scutil(
            "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.local\n  }\n  FTPPassive : 1\n}\n",
        );
        assert!(none.is_empty());
    }

    #[test]
    fn windows_settings_give_one_proxy_or_one_per_scheme() {
        let one =
            "\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings
    ProxyEnable    REG_DWORD    0x1
    ProxyServer    REG_SZ    proxy.corp.example:8080
    ProxyOverride    REG_SZ    <local>;*.corp.example;10.*
";
        let settings = ProxySettings::from_reg_query(one);
        assert_eq!(
            settings.proxy_for(false).as_deref(),
            Some("http://proxy.corp.example:8080")
        );
        assert_eq!(settings.https, settings.http);
        assert!(settings.bypasses("git.corp.example"));
        assert!(settings.bypasses("10.1.2.3"));
        let split = one.replace(
            "proxy.corp.example:8080",
            "http=web:80;https=secure:443;socks=sock:1080",
        );
        let settings = ProxySettings::from_reg_query(&split);
        assert_eq!(settings.http.as_deref(), Some("http://web:80"));
        assert_eq!(settings.https.as_deref(), Some("http://secure:443"));
        assert_eq!(settings.all.as_deref(), Some("socks5://sock:1080"));
        let disabled = one.replace("0x1", "0x0");
        assert!(ProxySettings::from_reg_query(&disabled).is_empty());
    }

    #[test]
    fn environment_variables_win_over_the_system_settings() {
        let vars = |name: &str| (name == "HTTPS_PROXY").then(|| "http://env:1".to_owned());
        let merged = ProxySettings::from_vars(vars).or(ProxySettings::from_scutil(SCUTIL));
        assert_eq!(merged.proxy_for(true).as_deref(), Some("http://env:1"));
        assert_eq!(
            merged.proxy_for(false).as_deref(),
            Some("http://proxy.corp.example:3128"),
            "the system fills what the environment leaves unset"
        );
        let no_proxy = |name: &str| (name == "NO_PROXY").then(|| "internal.example".to_owned());
        let merged = ProxySettings::from_vars(no_proxy).or(ProxySettings::from_scutil(SCUTIL));
        assert_eq!(merged.bypass, vec!["internal.example".to_owned()]);
    }
}
