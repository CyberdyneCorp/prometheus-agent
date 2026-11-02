//! The system clipboard for `/paste` and `Ctrl+V` (spec: tui, Clipboard
//! image attachments), through the platform's own tool: the macOS
//! pasteboard (`osascript`, `pbpaste`), the Windows clipboard (PowerShell,
//! also from WSL2 through `powershell.exe`), Wayland (`wl-paste`) and X11
//! (`xclip`, or `xsel` for text). Where none is reachable, for example
//! over SSH without a display, [`detect`] says why.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use pmt_core::env::LaunchEnv;
use pmt_tools::terminal::shell::find_executable;

/// What [`detect`] reads the environment for.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // independent facts of the environment
pub(super) struct Env {
    /// `macos`, `windows` or another OS (`linux`, the BSDs).
    pub(super) os: &'static str,
    /// `SSH_CONNECTION` or `SSH_TTY` is set.
    pub(super) ssh: bool,
    /// `WAYLAND_DISPLAY` is set.
    pub(super) wayland: bool,
    /// `DISPLAY` is set.
    pub(super) x11: bool,
    /// Running under WSL.
    pub(super) wsl: bool,
    /// `PATH`.
    pub(super) path: OsString,
    /// `PATHEXT` (Windows).
    pub(super) pathext: Option<OsString>,
}

impl Env {
    /// This process's environment.
    pub(super) fn of(launch: &LaunchEnv) -> Self {
        let set = |name: &str| launch.inherited(name).is_some_and(|v| !v.is_empty());
        let os = if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(windows) {
            "windows"
        } else {
            "linux"
        };
        let wsl = os == "linux"
            && std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|r| r.to_lowercase().contains("microsoft"));
        Self {
            os,
            ssh: set("SSH_CONNECTION") || set("SSH_TTY"),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            wsl,
            path: launch
                .inherited("PATH")
                .map(OsString::from)
                .unwrap_or_default(),
            pathext: launch.inherited("PATHEXT").map(OsString::from),
        }
    }

    fn find(&self, name: &str) -> Option<PathBuf> {
        find_executable(name, &self.path, self.pathext.as_deref())
    }
}

/// A reachable clipboard tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Clipboard {
    /// `osascript` for images, `pbpaste` for text.
    MacOs { osascript: PathBuf },
    /// PowerShell (`powershell.exe` from WSL).
    Windows { powershell: PathBuf },
    /// `wl-paste`.
    Wayland { wl_paste: PathBuf },
    /// `xclip`.
    Xclip { xclip: PathBuf },
    /// `xsel`: text only.
    Xsel { xsel: PathBuf },
}

/// The clipboard tool this environment reaches, or why there is none.
///
/// # Errors
///
/// The reason no clipboard is reachable.
pub(super) fn detect(env: &Env) -> Result<Clipboard, String> {
    let missing = |what: &str| format!("{what} was not found on PATH");
    match env.os {
        _ if env.ssh && env.os != "linux" => {
            Err("running over SSH, the clipboard here is not yours".to_owned())
        }
        "macos" => env
            .find("osascript")
            .map(|osascript| Clipboard::MacOs { osascript })
            .ok_or_else(|| missing("osascript")),
        "windows" => env
            .find("powershell")
            .map(|powershell| Clipboard::Windows { powershell })
            .ok_or_else(|| missing("powershell")),
        _ => unix(env),
    }
}

fn unix(env: &Env) -> Result<Clipboard, String> {
    if env.wsl
        && let Some(powershell) = env.find("powershell.exe")
    {
        return Ok(Clipboard::Windows { powershell });
    }
    if !env.wayland && !env.x11 {
        let over = if env.ssh { " (over SSH)" } else { "" };
        return Err(format!(
            "no display{over}: WAYLAND_DISPLAY and DISPLAY are unset"
        ));
    }
    if env.wayland
        && let Some(wl_paste) = env.find("wl-paste")
    {
        return Ok(Clipboard::Wayland { wl_paste });
    }
    if env.x11 {
        if let Some(xclip) = env.find("xclip") {
            return Ok(Clipboard::Xclip { xclip });
        }
        if let Some(xsel) = env.find("xsel") {
            return Ok(Clipboard::Xsel { xsel });
        }
    }
    Err("no clipboard tool: install wl-clipboard (Wayland) or xclip (X11)".to_owned())
}

const WINDOWS_IMAGE: &str = "Add-Type -AssemblyName System.Windows.Forms; \
    $i = [System.Windows.Forms.Clipboard]::GetImage(); \
    if ($i) { $m = New-Object System.IO.MemoryStream; \
    $i.Save($m, [System.Drawing.Imaging.ImageFormat]::Png); \
    [Convert]::ToBase64String($m.ToArray()) }";

impl Clipboard {
    /// The clipboard's image as PNG bytes; `None` when it holds none.
    ///
    /// # Errors
    ///
    /// The tool cannot read images (`xsel`) or failed to run.
    pub(super) fn image(&self) -> Result<Option<Vec<u8>>, String> {
        match self {
            Self::MacOs { osascript } => {
                let out = run(osascript, &["-e", "the clipboard as «class PNGf»"]);
                Ok(out.and_then(|o| from_applescript(&String::from_utf8_lossy(&o))))
            }
            Self::Windows { powershell } => {
                let out = run(powershell, &["-NoProfile", "-Command", WINDOWS_IMAGE]);
                Ok(out.and_then(|o| {
                    use base64::Engine as _;
                    let text = String::from_utf8_lossy(&o).trim().to_owned();
                    base64::engine::general_purpose::STANDARD.decode(text).ok()
                }))
            }
            Self::Wayland { wl_paste } => Ok(typed_image(wl_paste, &["--list-types"], |t| {
                vec!["--type".into(), t.into()]
            })),
            Self::Xclip { xclip } => {
                let targets = ["-selection", "clipboard", "-t", "TARGETS", "-o"];
                Ok(typed_image(xclip, &targets, |t| {
                    ["-selection", "clipboard", "-t", t, "-o"]
                        .map(str::to_owned)
                        .to_vec()
                }))
            }
            Self::Xsel { .. } => {
                Err("xsel cannot read images; install xclip or wl-clipboard".to_owned())
            }
        }
    }

    /// The clipboard's text; `None` when it holds none.
    pub(super) fn text(&self) -> Option<String> {
        let out = match self {
            Self::MacOs { .. } => run(&PathBuf::from("pbpaste"), &[]),
            Self::Windows { powershell } => run(
                powershell,
                &["-NoProfile", "-Command", "Get-Clipboard -Raw"],
            ),
            Self::Wayland { wl_paste } => run(wl_paste, &["--no-newline", "--type", "text"]),
            Self::Xclip { xclip } => run(xclip, &["-selection", "clipboard", "-o"]),
            Self::Xsel { xsel } => run(xsel, &["--clipboard", "--output"]),
        }?;
        let text = String::from_utf8(out).ok()?;
        (!text.trim().is_empty()).then_some(text)
    }
}

/// The first image type `list` offers, read with `read(type)`.
fn typed_image(
    tool: &PathBuf,
    list: &[&str],
    read: impl Fn(&str) -> Vec<String>,
) -> Option<Vec<u8>> {
    let types = String::from_utf8(run(tool, list)?).ok()?;
    let wanted = ["image/png", "image/jpeg", "image/webp", "image/gif"]
        .into_iter()
        .find(|t| types.lines().any(|l| l.trim() == *t))?;
    let args = read(wanted);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(tool, &args).filter(|bytes| !bytes.is_empty())
}

/// Runs `tool`; its stdout when it succeeds.
fn run(tool: &PathBuf, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// The `«data PNGf<hex>»` that `osascript` prints for an image.
fn from_applescript(text: &str) -> Option<Vec<u8>> {
    let hex = text.trim().strip_prefix("«data PNGf")?.strip_suffix('»')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

/// What `/paste` prints when no clipboard is reachable.
pub(super) fn unavailable(reason: &str) -> String {
    format!("clipboard image unavailable here: {reason}; attach a file path with @path instead")
}

#[cfg(test)]
mod tests {
    use super::{Clipboard, Env, detect, from_applescript, unavailable};

    fn bin_with(tools: &[&str]) -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().unwrap();
        for tool in tools {
            let path = dir.path().join(tool);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let env = Env {
            os: "linux",
            path: dir.path().as_os_str().to_owned(),
            ..Env::default()
        };
        (dir, env)
    }

    /// Scenario: Over SSH (the detection side).
    #[test]
    fn over_ssh_without_a_display_there_is_no_clipboard() {
        let (_dir, mut env) = bin_with(&["xclip", "wl-paste"]);
        env.ssh = true;
        let why = detect(&env).unwrap_err();
        assert_eq!(
            why,
            "no display (over SSH): WAYLAND_DISPLAY and DISPLAY are unset"
        );
        assert_eq!(
            unavailable(&why),
            "clipboard image unavailable here: no display (over SSH): WAYLAND_DISPLAY and DISPLAY are unset; attach a file path with @path instead"
        );
        env.os = "macos";
        assert!(detect(&env).unwrap_err().contains("over SSH"));
    }

    #[cfg(unix)]
    #[test]
    fn wayland_then_x11_tools_are_preferred_in_order() {
        let (dir, mut env) = bin_with(&["xclip", "wl-paste", "xsel"]);
        env.x11 = true;
        assert_eq!(
            detect(&env),
            Ok(Clipboard::Xclip {
                xclip: dir.path().join("xclip")
            })
        );
        env.wayland = true;
        assert_eq!(
            detect(&env),
            Ok(Clipboard::Wayland {
                wl_paste: dir.path().join("wl-paste")
            })
        );
        let (dir, mut env) = bin_with(&["xsel"]);
        env.x11 = true;
        let xsel = detect(&env).unwrap();
        assert_eq!(
            xsel,
            Clipboard::Xsel {
                xsel: dir.path().join("xsel")
            }
        );
        assert!(
            xsel.image()
                .unwrap_err()
                .contains("xsel cannot read images")
        );
        let (dir, mut env) = bin_with(&["powershell.exe"]);
        env.wsl = true;
        assert_eq!(
            detect(&env),
            Ok(Clipboard::Windows {
                powershell: dir.path().join("powershell.exe")
            })
        );
    }

    #[test]
    fn applescript_png_data_decodes() {
        assert_eq!(
            from_applescript("«data PNGf89504E47»\n"),
            Some(vec![0x89, 0x50, 0x4e, 0x47])
        );
        assert_eq!(from_applescript("error"), None);
    }
}
