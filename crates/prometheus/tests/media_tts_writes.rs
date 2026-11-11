//! `text_to_speech` writes are claimed writes (spec: security-boundaries,
//! Protected instruction files; tool-system, Resource claims): a write to
//! a protected instruction file asks for approval (denied in a one-shot
//! run) and a device-namespace path is refused before any filesystem
//! access, as for `write_file`.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Stdio;

use common::{append_config, command, mock_home, stderr};

/// A one-shot turn calling `text_to_speech` with `output_path` (a command
/// voice; the gate decides before it would run); how it ended and the
/// tool results.
fn speak_to(home: &Path, output_path: &str) -> (std::process::Output, String) {
    let script = home.join("tts.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"text_to_speech\"\n\
             arguments = {{ text = \"hello\", output_path = '{output_path}' }}\n\n\
             [[rule]]\nstep = 1\ntext = \"done\"\n"
        ),
    )
    .unwrap();
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let out = command(home)
        .current_dir(&work)
        .args([
            "--json",
            "-z",
            "speak",
            "--toolsets",
            "tts",
            "--mock-script",
        ])
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let results = common::session_messages(home, &common::newest_session(home))
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    (out, results)
}

fn tts_home() -> tempfile::TempDir {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[tts]\nprovider = \"fake\"\n\n[tts.providers.fake]\ntype = \"command\"\n\
         command = \"cp {input_path} {output_path}\"\n",
    );
    home
}

#[test]
fn a_protected_output_path_needs_approval_and_a_unc_path_is_refused() {
    let home = tts_home();
    let h = home.path();
    // A protected instruction file asks first; a one-shot run cannot ask.
    let (out, _) = speak_to(h, "AGENTS.md");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("chat.approval_required"),
        "{}",
        stderr(&out)
    );
    assert!(
        !h.join("work/AGENTS.md").exists(),
        "the protected file was written"
    );

    let (out, result) = speak_to(h, r"\\attacker.example\share\x.mp3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(result.to_lowercase().contains("unc"), "{result}");
}

/// Regression: when the session's sandbox cannot be set up (here
/// `security.sandbox.mode = "off"` without `allow_degraded = ["all"]`), a
/// command voice is refused, never run through a plain shell.
#[test]
fn a_command_voice_is_refused_without_a_sandbox() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[security.sandbox]\nmode = \"off\"\n\n[tts]\nprovider = \"fake\"\n\n\
         [tts.providers.fake]\ntype = \"command\"\n\
         command = \"touch ran-unconfined && cp {input_path} {output_path}\"\n",
    );
    let h = home.path();
    let (out, result) = speak_to(h, "greeting.mp3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(result.contains("sandbox unavailable"), "{result}");
    assert!(!h.join("work/ran-unconfined").exists(), "the command ran");
    assert!(!h.join("work/greeting.mp3").exists());
}

/// Regression: a command voice runs inside the session's sandbox and can
/// still read the text it is given and write the audio it makes (spec:
/// web-and-media-tools, Command-template speech providers).
#[test]
fn a_command_voice_speaks_inside_the_session_sandbox() {
    let home = tts_home();
    let h = home.path();
    let (out, result) = speak_to(h, "greeting.mp3");
    assert!(out.status.success(), "{}", stderr(&out));
    let spoken = h.join("work/greeting.mp3");
    assert!(spoken.exists(), "{result}");
    assert_eq!(std::fs::read_to_string(spoken).unwrap(), "hello");
}
