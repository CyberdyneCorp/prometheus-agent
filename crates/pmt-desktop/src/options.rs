//! The `[desktop]` section of the active profile's `config.toml`, read by
//! the shell before its first window opens (spec: desktop-app, Desktop
//! launch options; Closing windows does not stop the agent; Global hotkey
//! quick-ask).
//!
//! It is read leniently on purpose: a typo in a launch option must not
//! keep the app from starting, so an invalid value becomes a notice naming
//! the key and its allowed values, and that option's default applies. The
//! config schema keeps these keys as strings for the same reason (the
//! daemon must not refuse the profile over a desktop option).

use std::path::Path;

use toml::{Table, Value};

/// `desktop.disable_gpu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisableGpu {
    /// Disabled only on remote displays.
    #[default]
    Auto,
    /// Always disabled.
    On,
    /// Never disabled.
    Off,
}

/// `desktop.linux_window_backend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowBackend {
    /// The toolkit's choice.
    #[default]
    Auto,
    /// Native Wayland.
    Wayland,
    /// X11 (`XWayland` on a Wayland session).
    X11,
}

/// `desktop.linux_secret_store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecretStore {
    /// Whatever the session offers.
    #[default]
    Auto,
    /// The freedesktop Secret Service.
    SecretService,
    /// `KWallet`.
    Kwallet,
}

/// `[desktop.quick_ask]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAsk {
    /// The global chord; empty disables the hotkey.
    pub hotkey: String,
    /// The profile asked; `None` is the last used one.
    pub profile: Option<String>,
    /// A quick-ask session active within this many seconds is reused.
    pub reuse_secs: u64,
}

/// The default quick-ask chord.
pub const DEFAULT_HOTKEY: &str = "CommandOrControl+Shift+Space";

impl Default for QuickAsk {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.to_owned(),
            profile: None,
            reuse_secs: 900,
        }
    }
}

/// `[desktop.hud]` (spec: desktop-app, HUD mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hud {
    /// The global chord that snaps the HUD to the pointer; empty disables
    /// it.
    pub snap_hotkey: String,
    /// A tap (left click) on the tray icon summons the HUD.
    pub tap_to_summon: bool,
}

/// The default snap chord.
pub const DEFAULT_SNAP_HOTKEY: &str = "CommandOrControl+Shift+G";

impl Default for Hud {
    fn default() -> Self {
        Self {
            snap_hotkey: DEFAULT_SNAP_HOTKEY.to_owned(),
            tap_to_summon: false,
        }
    }
}

/// The desktop options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopOptions {
    /// Keep running in the tray after the last window closes.
    pub keep_in_tray: bool,
    /// The quick-ask hotkey.
    pub quick_ask: QuickAsk,
    /// GPU acceleration.
    pub disable_gpu: DisableGpu,
    /// The Linux window system backend.
    pub linux_window_backend: WindowBackend,
    /// The Linux secret store.
    pub linux_secret_store: SecretStore,
    /// The webview's accessibility tree.
    pub accessibility_tree: bool,
    /// Keep the Linux XDG launcher entry current.
    pub manage_launcher_entry: bool,
    /// HUD mode.
    pub hud: Hud,
}

impl Default for DesktopOptions {
    fn default() -> Self {
        Self {
            keep_in_tray: true,
            quick_ask: QuickAsk::default(),
            disable_gpu: DisableGpu::Auto,
            linux_window_backend: WindowBackend::Auto,
            linux_secret_store: SecretStore::Auto,
            accessibility_tree: true,
            manage_launcher_entry: true,
            hud: Hud::default(),
        }
    }
}

/// The options and the notices of the values that were not valid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Read {
    /// What applies.
    pub options: DesktopOptions,
    /// One line per invalid value.
    pub notices: Vec<String>,
}

/// Reads the `[desktop]` section of the `config.toml` at `path`; a missing
/// file is all defaults.
#[must_use]
pub fn read(path: &Path) -> Read {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => Read::default(),
    }
}

/// Parses the `[desktop]` section of config text.
#[must_use]
pub fn parse(text: &str) -> Read {
    let mut read = Read::default();
    let Ok(root) = text.parse::<Table>() else {
        // The daemon reports an invalid file; the desktop starts anyway.
        return read;
    };
    let Some(Value::Table(desktop)) = root.get("desktop") else {
        return read;
    };
    let notices = &mut read.notices;
    let o = &mut read.options;
    o.keep_in_tray = flag(desktop, "keep_in_tray", true, notices);
    o.accessibility_tree = flag(desktop, "accessibility_tree", true, notices);
    o.manage_launcher_entry = flag(desktop, "manage_launcher_entry", true, notices);
    o.disable_gpu = choice(
        desktop,
        "disable_gpu",
        &[
            ("auto", DisableGpu::Auto),
            ("on", DisableGpu::On),
            ("off", DisableGpu::Off),
        ],
        notices,
    );
    o.linux_window_backend = choice(
        desktop,
        "linux_window_backend",
        &[
            ("auto", WindowBackend::Auto),
            ("wayland", WindowBackend::Wayland),
            ("x11", WindowBackend::X11),
        ],
        notices,
    );
    o.linux_secret_store = choice(
        desktop,
        "linux_secret_store",
        &[
            ("auto", SecretStore::Auto),
            ("secret-service", SecretStore::SecretService),
            ("kwallet", SecretStore::Kwallet),
        ],
        notices,
    );
    if let Some(Value::Table(quick)) = desktop.get("quick_ask") {
        o.quick_ask = quick_ask(quick, notices);
    }
    if let Some(Value::Table(hud)) = desktop.get("hud") {
        o.hud = hud_options(hud, notices);
    }
    read
}

fn hud_options(table: &Table, notices: &mut Vec<String>) -> Hud {
    let mut h = Hud::default();
    match table.get("snap_hotkey") {
        None => {}
        Some(Value::String(chord)) => chord.trim().clone_into(&mut h.snap_hotkey),
        Some(_) => notices.push("desktop.hud.snap_hotkey: expected a chord such as \"CommandOrControl+Shift+G\" or \"\"".to_owned()),
    }
    match table.get("tap_to_summon") {
        None => {}
        Some(Value::Boolean(b)) => h.tap_to_summon = *b,
        Some(_) => notices.push("desktop.hud.tap_to_summon: expected true or false".to_owned()),
    }
    h
}

fn quick_ask(table: &Table, notices: &mut Vec<String>) -> QuickAsk {
    let mut q = QuickAsk::default();
    match table.get("hotkey") {
        None => {}
        Some(Value::String(chord)) => chord.trim().clone_into(&mut q.hotkey),
        Some(_) => notices.push("desktop.quick_ask.hotkey: expected a chord such as \"CommandOrControl+Shift+Space\" or \"\"".to_owned()),
    }
    match table.get("profile") {
        None => {}
        Some(Value::String(p)) => q.profile = Some(p.clone()).filter(|p| !p.is_empty()),
        Some(_) => notices.push("desktop.quick_ask.profile: expected a profile name".to_owned()),
    }
    match table.get("reuse_secs") {
        None => {}
        Some(Value::Integer(n)) if *n >= 0 => q.reuse_secs = n.unsigned_abs(),
        Some(_) => {
            notices.push("desktop.quick_ask.reuse_secs: expected a number of seconds".to_owned());
        }
    }
    q
}

fn flag(table: &Table, key: &str, default: bool, notices: &mut Vec<String>) -> bool {
    match table.get(key) {
        None => default,
        Some(Value::Boolean(b)) => *b,
        Some(_) => {
            notices.push(format!("desktop.{key}: expected true or false"));
            default
        }
    }
}

fn choice<T: Copy + Default>(
    table: &Table,
    key: &str,
    allowed: &[(&str, T)],
    notices: &mut Vec<String>,
) -> T {
    let Some(value) = table.get(key) else {
        return T::default();
    };
    if let Some((_, found)) = value
        .as_str()
        .and_then(|text| allowed.iter().find(|(name, _)| *name == text))
    {
        return *found;
    }
    let names: Vec<String> = allowed.iter().map(|(n, _)| format!("\"{n}\"")).collect();
    notices.push(format!(
        "desktop.{key}: expected one of {}",
        names.join(", ")
    ));
    T::default()
}

/// What the display looks like, for the `auto` choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Display {
    /// An X11 forwarding, SSH or RDP session.
    pub remote: bool,
}

impl Display {
    /// The display of this process, from its environment.
    #[must_use]
    pub fn detect() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let forwarded_x = var("DISPLAY").is_some_and(|d| {
            let host = d.split(':').next().unwrap_or_default();
            !host.is_empty() && host != "unix"
        });
        let rdp = var("SESSIONNAME").is_some_and(|s| s.starts_with("RDP-"));
        Self {
            remote: forwarded_x || rdp || var("SSH_CONNECTION").is_some(),
        }
    }
}

impl DesktopOptions {
    /// The environment the webview toolkit reads, set before the first
    /// window opens: GPU, window backend and accessibility switches for
    /// `os` (`linux`, `windows`; macOS's `WKWebView` has none of them).
    #[must_use]
    pub fn environment(&self, os: &str, display: Display) -> Vec<(&'static str, String)> {
        let gpu_off = match self.disable_gpu {
            DisableGpu::On => true,
            DisableGpu::Off => false,
            DisableGpu::Auto => display.remote,
        };
        let mut env = Vec::new();
        match os {
            "linux" => {
                if gpu_off {
                    env.push(("WEBKIT_DISABLE_COMPOSITING_MODE", "1".to_owned()));
                    env.push(("WEBKIT_DISABLE_DMABUF_RENDERER", "1".to_owned()));
                }
                match self.linux_window_backend {
                    WindowBackend::Auto => {}
                    WindowBackend::Wayland => env.push(("GDK_BACKEND", "wayland".to_owned())),
                    WindowBackend::X11 => env.push(("GDK_BACKEND", "x11".to_owned())),
                }
                if !self.accessibility_tree {
                    env.push(("NO_AT_BRIDGE", "1".to_owned()));
                }
            }
            "windows" => {
                let mut args = Vec::new();
                if gpu_off {
                    args.push("--disable-gpu");
                }
                if !self.accessibility_tree {
                    args.push("--disable-renderer-accessibility");
                }
                if !args.is_empty() {
                    env.push(("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", args.join(" ")));
                }
            }
            _ => {}
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::{DisableGpu, Display, WindowBackend, parse};

    #[test]
    fn a_typo_in_a_launch_option_launches_with_auto_and_a_notice() {
        let read = parse("[desktop]\ndisable_gpu = \"yes\"\n");
        assert_eq!(read.options.disable_gpu, DisableGpu::Auto);
        assert_eq!(
            read.notices,
            [r#"desktop.disable_gpu: expected one of "auto", "on", "off""#]
        );
    }

    #[test]
    fn x11_on_a_wayland_session_opens_through_xwayland() {
        let read = parse("[desktop]\nlinux_window_backend = \"x11\"\n");
        assert_eq!(read.options.linux_window_backend, WindowBackend::X11);
        let env = read.options.environment("linux", Display::default());
        assert!(env.contains(&("GDK_BACKEND", "x11".to_owned())));
    }

    #[test]
    fn auto_disables_the_gpu_only_on_remote_displays() {
        let options = parse("").options;
        assert!(options.environment("linux", Display::default()).is_empty());
        let remote = options.environment("linux", Display { remote: true });
        assert!(remote.contains(&("WEBKIT_DISABLE_COMPOSITING_MODE", "1".to_owned())));
        let windows = parse("[desktop]\ndisable_gpu = \"on\"\naccessibility_tree = false\n")
            .options
            .environment("windows", Display::default());
        assert_eq!(
            windows,
            [(
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                "--disable-gpu --disable-renderer-accessibility".to_owned()
            )]
        );
    }

    #[test]
    fn quick_ask_and_tray_options_have_their_defaults() {
        let read = parse(
            "[desktop]\nkeep_in_tray = false\n[desktop.quick_ask]\nhotkey = \"\"\nreuse_secs = 60\n",
        );
        assert!(!read.options.keep_in_tray);
        assert_eq!(read.options.quick_ask.hotkey, "");
        assert_eq!(read.options.quick_ask.reuse_secs, 60);
        assert!(read.notices.is_empty());
    }
}
