//! Pastes, the clipboard, math and the terminal's background in the TUI
//! on a pseudo-terminal (spec: tui, Large pastes collapse into chips;
//! Clipboard image attachments; LaTeX math rendering; Terminal background
//! detection). The platform clipboard tools are fakes on `PATH`
//! (`osascript` on macOS, `wl-paste` elsewhere).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::tui::{ENTER, enter, finish, opened, see, tui, tui_with};
use common::{PNG, daemon, mock_home};
use expectrl::Expect;

/// The display variables a clipboard test controls itself.
const DISPLAY_VARS: &[&str] = &["SSH_CONNECTION", "SSH_TTY", "DISPLAY", "WAYLAND_DISPLAY"];

fn user_messages(home: &Path) -> Vec<serde_json::Value> {
    common::session_messages(home, &common::newest_session(home))
        .into_iter()
        .filter(|m| m["role"] == "user")
        .collect()
}

/// A directory with fake clipboard tools holding [`PNG`], and the `PATH`
/// that finds them first.
fn fake_clipboard(home: &Path) -> String {
    let bin = home.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let png = home.join("clip.png");
    std::fs::write(&png, PNG).unwrap();
    let hex = PNG.iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02X}");
        hex
    });
    common::pty::script(&bin, "osascript", &format!("printf '«data PNGf{hex}»\\n'"));
    common::pty::script(
        &bin,
        "wl-paste",
        &format!(
            "case \"$1\" in --list-types) echo image/png ;; --type) cat '{}' ;; *) exit 1 ;; esac",
            png.display()
        ),
    );
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Scenario: Long log paste.
#[test]
fn a_300_line_paste_is_one_chip_and_the_prompt_carries_every_line() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    let log: String = (1..=300)
        .map(|n| format!("log line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    p.send(format!("\u{1b}[200~{log}\u{1b}[201~")).unwrap();
    see(&mut p, "[Pasted text #1: 300 lines]");
    enter(&mut p, " what failed?");
    see(&mut p, "what failed?");
    p.expect("log line 300").unwrap();
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let users = user_messages(home.path());
    let sent = users[0]["content"].as_str().unwrap();
    assert_eq!(
        sent.lines().filter(|l| l.starts_with("log line ")).count(),
        300
    );
    assert!(sent.ends_with("what failed?"), "{sent}");
    assert!(
        !sent.contains("[Pasted text"),
        "the chip is replaced by its text"
    );
}

/// Scenario: Image in the clipboard.
#[test]
fn paste_attaches_the_clipboard_image_to_the_next_prompt() {
    let home = mock_home();
    let path = fake_clipboard(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui_with(
        home.path(),
        &[],
        &[("PATH", &path), ("WAYLAND_DISPLAY", "wayland-test")],
        DISPLAY_VARS,
    );
    opened(&mut p);
    enter(&mut p, "/paste");
    see(&mut p, "[Image #1] attached; Backspace removes it");
    enter(&mut p, "describe it");
    see(&mut p, "mock: describe it");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let users = user_messages(home.path());
    let images = &users[0]["metadata"]["images"];
    let name = images[0]["name"].as_str().unwrap_or_default();
    assert!(
        name.starts_with("clipboard-")
            && Path::new(name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("png")),
        "{users:?}"
    );
    assert_eq!(images[0]["media_type"], "image/png");
    assert_eq!(users[0]["content"], "describe it", "the badge is not text");
}

#[test]
fn backspace_removes_the_image_badge() {
    let home = mock_home();
    let path = fake_clipboard(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui_with(
        home.path(),
        &[],
        &[("PATH", &path), ("WAYLAND_DISPLAY", "wayland-test")],
        DISPLAY_VARS,
    );
    opened(&mut p);
    enter(&mut p, "/paste");
    see(&mut p, "[Image #1] attached");
    // The space after the badge, then the badge.
    p.send("\u{7f}\u{7f}").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    enter(&mut p, "no image");
    see(&mut p, "mock: no image");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let users = user_messages(home.path());
    assert!(users[0]["metadata"]["images"].is_null(), "{users:?}");
}

/// Scenario: Over SSH.
#[test]
fn over_ssh_without_a_display_paste_explains_and_attaches_nothing() {
    let home = mock_home();
    let path = fake_clipboard(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui_with(
        home.path(),
        &[],
        &[
            ("PATH", &path),
            ("SSH_CONNECTION", "10.0.0.2 52000 10.0.0.1 22"),
        ],
        DISPLAY_VARS,
    );
    opened(&mut p);
    enter(&mut p, "/paste");
    see(&mut p, "clipboard image unavailable here:");
    see(&mut p, "attach a file path with @path instead");
    enter(&mut p, "plain");
    see(&mut p, "mock: plain");
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(!text.contains("[Image #"));
    let users = user_messages(home.path());
    assert!(users[0]["metadata"]["images"].is_null(), "{users:?}");
}

#[test]
fn terminal_setup_shows_asks_and_adds_the_binding_with_a_backup() {
    let home = mock_home();
    let user = common::user_home(home.path());
    let settings = if cfg!(target_os = "macos") {
        user.join("Library/Application Support/Code/User")
    } else {
        user.join(".config/Code/User")
    };
    std::fs::create_dir_all(&settings).unwrap();
    let file = settings.join("keybindings.json");
    std::fs::write(&file, "// mine\n[\n]\n").unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui_with(home.path(), &[], &[], &["XDG_CONFIG_HOME", "APPDATA"]);
    opened(&mut p);
    enter(&mut p, "/terminal-setup");
    see(&mut p, "workbench.action.terminal.sendSequence");
    see(&mut p, "add the Ctrl+V keybinding to 1 file(s)?");
    p.send("y").unwrap();
    see(&mut p, "VS Code: added Ctrl+V");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(written.starts_with("// mine"), "{written}");
    assert!(written.contains(r#""text": "\u0016""#), "{written}");
    let backup = std::fs::read_to_string(settings.join("keybindings.json.bak")).unwrap();
    assert_eq!(backup, "// mine\n[\n]\n");
}

/// Scenario: Unsupported macro (and the formatted rest of the answer).
#[test]
fn answers_render_math_and_show_unsupported_tex_literally() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, r"$x^2$ but $\begin{tikzpicture}$ costs $5 and $10");
    see(
        &mut p,
        r"mock: x² but $\begin{tikzpicture}$ costs $5 and $10",
    );
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

/// The line interface prints the TeX as it came.
#[test]
fn the_line_interface_prints_raw_tex() {
    let home = mock_home();
    let mut p = common::pty::spawn(home.path(), &[]);
    common::pty::prompt(&mut p);
    p.send(format!("{}\r", r"area is $\pi r^2$")).unwrap();
    p.expect(r"mock: area is $\pi r^2$").unwrap();
    common::pty::exit(p);
}

/// Scenario: Light terminal without COLORFGBG.
#[test]
fn a_white_osc11_answer_picks_the_light_theme() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    p.expect(ENTER).unwrap();
    p.expect("\u{1b}]11;?").unwrap();
    p.send("\u{1b}]11;rgb:ffff/ffff/ffff\u{1b}\\").unwrap();
    see(&mut p, "default");
    enter(&mut p, "/theme");
    see(&mut p, "theme: light (auto: detected)");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

#[test]
fn colorfgbg_decides_first_and_an_explicit_theme_skips_detection() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    // Everything written from the alternate screen to `/theme`'s answer.
    let theme_of = |args: &[&str], env: &[(&str, &str)]| {
        let mut p = tui(home.path(), args, env);
        p.expect(ENTER).unwrap();
        let mut shown = see(&mut p, "default");
        enter(&mut p, "/theme");
        shown += &see(&mut p, "theme:");
        shown += &see(&mut p, ")");
        enter(&mut p, "/quit");
        assert_eq!(finish(p).1, Some(0));
        shown
    };
    let shown = theme_of(&[], &[("COLORFGBG", "0;15")]);
    assert!(
        shown.contains("light") && shown.contains("detected"),
        "{shown}"
    );
    assert!(
        !shown.contains("\u{1b}]11;?"),
        "COLORFGBG answers without a query"
    );
    // No answer to the query: dark, within the 100 ms limit.
    let shown = theme_of(&[], &[]);
    assert!(
        shown.contains("\u{1b}]11;?") && shown.contains("dark"),
        "{shown}"
    );
    let shown = theme_of(&["--set", "ui.tui.theme=light"], &[]);
    assert!(shown.contains("ui.tui.theme"), "{shown}");
    assert!(
        !shown.contains("\u{1b}]11;?"),
        "an explicit theme is never queried"
    );
}
