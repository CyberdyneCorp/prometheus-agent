//! `read_file`: pagination, dedup, guards, encodings, conflict markers,
//! path suggestions, secret files and device-namespace paths.

use pmt_tools::files::{Kind, Linter, ReadArgs};
use pmt_tools::result::Status;

use crate::support::{Fixture, body, read_all};

fn numbered(lines: std::ops::RangeInclusive<usize>) -> String {
    lines
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn pages_default_to_2000_lines_and_give_the_next_offset() {
    let fx = Fixture::new();
    fx.write("big.txt", numbered(1..=2500));
    let session = fx.session();
    let first = read_all(&session, "big.txt");
    assert_eq!(first["lines"], "1-2000");
    assert_eq!(first["next_offset"], 2001);
    assert_eq!(first["total_lines"], 2500);
    assert!(
        first["content"]
            .as_str()
            .unwrap()
            .starts_with("     1|line 1\n")
    );
    let next = body(&session.read_file(&ReadArgs {
        offset: 2001,
        ..ReadArgs::new("big.txt")
    }));
    assert_eq!(next["lines"], "2001-2500");
    assert!(next.get("next_offset").is_none());
}

#[test]
fn the_character_cap_stops_on_a_line_and_returns_next_offset() {
    let fx = Fixture::new();
    // 1,500 lines of 80 characters pass the default 100,000-character cap
    // well before the 2,000-line limit.
    fx.write("wide.txt", format!("{}\n", "y".repeat(80)).repeat(1500));
    let page = read_all(&fx.session(), "wide.txt");
    let content = page["content"].as_str().unwrap();
    assert!(content.chars().count() <= 100_000);
    let next = page["next_offset"].as_u64().unwrap();
    assert!(next > 1 && next < 1500, "{next}");
    assert_eq!(page["lines"], format!("1-{}", next - 1));
    assert_eq!(content.lines().count(), usize::try_from(next - 1).unwrap());
}

#[test]
fn a_third_identical_read_is_blocked() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    let session = fx.session();
    let args = ReadArgs::new("a.rs");
    assert!(body(&session.read_file(&args))["content"].is_string());
    let second = session.read_file(&args);
    assert_eq!(body(&second)["status"], "unchanged");
    let third = session.read_file(&args);
    assert_eq!(third.status, Status::Blocked);
    assert!(
        third
            .text_content()
            .starts_with("BLOCKED: You have already read this exact region"),
        "{}",
        third.text_content()
    );
    // A change on disk, or a compaction, makes the read new again.
    fx.write("a.rs", "fn b() {}\n");
    assert!(body(&session.read_file(&args))["content"].is_string());
    let _ = session.read_file(&args);
    session.note_compaction();
    assert!(body(&session.read_file(&args))["content"].is_string());
}

#[test]
fn binary_files_are_refused_and_utf16_is_decoded() {
    let fx = Fixture::new();
    fx.write("logo.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x01");
    let result = fx.session().read_file(&ReadArgs::new("logo.png"));
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        "Cannot read binary file 'logo.png'; use vision_analyze for images or terminal for binary inspection"
    );
    let text = "Installing component 1 of 3\r\nDone.\r\n";
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    fx.write("install.log", utf16);
    let page = read_all(&fx.session(), "install.log");
    assert_eq!(page["encoding"], "utf-16le");
    assert_eq!(
        page["content"],
        "     1|Installing component 1 of 3\n     2|Done.\n"
    );
}

#[test]
fn conflict_markers_are_counted_with_a_hint() {
    let fx = Fixture::new();
    fx.write(
        "merge.rs",
        "<<<<<<< HEAD\nlet a = 1;\n=======\nlet a = 2;\n>>>>>>> topic\nfn x() {}\n",
    );
    let page = read_all(&fx.session(), "merge.rs");
    assert_eq!(page["conflict_blocks"], 1);
    assert_eq!(
        page["hint"],
        "resolve the merge conflicts before editing around them"
    );
    fx.write("clean.rs", "fn x() {}\n");
    assert!(
        read_all(&fx.session(), "clean.rs")
            .get("conflict_blocks")
            .is_none()
    );
}

#[test]
fn a_typo_suggests_similar_paths() {
    let fx = Fixture::new();
    fx.write("src/config.rs", "");
    fx.write("src/main.rs", "");
    let result = fx.session().read_file(&ReadArgs::new("src/conifg.rs"));
    assert_eq!(result.status, Status::Error);
    let text = result.text_content();
    assert!(
        text.starts_with("File not found: src/conifg.rs\nDid you mean:"),
        "{text}"
    );
    assert!(text.contains("  src/config.rs"), "{text}");
    assert!(!text.contains("main.rs"), "{text}");
}

#[test]
fn project_dotenv_files_are_never_read_but_may_be_written() {
    let fx = Fixture::new();
    fx.write(".env.local", "OPENAI_API_KEY=sk-live-secret\n");
    fx.write(".env.example", "OPENAI_API_KEY=\n");
    let session = fx.session();
    let result = session.read_file(&ReadArgs::new(".env.local"));
    assert_eq!(result.status, Status::Denied);
    assert_eq!(
        result.text_content(),
        "Refusing to read '.env.local': it typically holds secrets; ask the user to share the specific value you need"
    );
    assert!(!result.text_content().contains("sk-live-secret"));
    assert_eq!(
        session.read_file(&ReadArgs::new(".env.example")).status,
        Status::Ok
    );
    let written = session.write_file(".env", "DEBUG=1\n");
    assert_eq!(written.status, Status::Ok, "{}", written.text_content());
}

#[test]
fn unc_and_device_paths_are_refused_before_any_access() {
    let fx = Fixture::new();
    let session = fx.session();
    for path in [r"\\attacker.example\share\x", r"\\.\pipe\x", r"\??\C:\x"] {
        let result = session.read_file(&ReadArgs::new(path));
        assert_eq!(result.status, Status::Denied);
        assert_eq!(
            result.text_content(),
            format!("Refusing device-namespace or UNC path '{path}'")
        );
        let write = session.write_file(path, "x");
        assert_eq!(write.status, Status::Denied);
    }
}

#[test]
fn paths_outside_the_capabilities_are_denied() {
    let fx = Fixture::new();
    let secret = fx.home.join("private.txt");
    std::fs::write(&secret, "private").unwrap();
    let result = fx
        .session()
        .read_file(&ReadArgs::new(secret.to_str().unwrap()));
    assert_eq!(result.status, Status::Denied, "{}", result.text_content());
    assert!(
        result.text_content().contains("fs.read"),
        "{}",
        result.text_content()
    );
}

#[cfg(unix)]
#[test]
fn devices_and_fifos_are_refused() {
    let fx = Fixture::new();
    let session = fx.session_with(Linter::builtin(), &["fs.read:/dev/**"]);
    let result = session.read_file(&ReadArgs::new("/dev/zero"));
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        "Cannot read '/dev/zero': this is a device file that would block or produce infinite output."
    );
    let fifo = fx.outside.join("pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let shown = fifo.to_str().unwrap();
    let result = session.read_file(&ReadArgs::new(shown));
    assert_eq!(
        result.text_content(),
        format!("Cannot read '{shown}': this is a named pipe that would block.")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn proc_environ_is_refused() {
    let fx = Fixture::new();
    let result = fx.session().read_file(&ReadArgs::new("/proc/self/environ"));
    assert_eq!(result.status, Status::Denied);
    assert!(result.text_content().contains("environment secrets"));
}

#[test]
fn list_dir_goes_through_the_backend_and_its_capabilities() {
    let fx = Fixture::new();
    fx.write("src/a.rs", "fn a() {}\n");
    fx.write("b.txt", "b");
    let session = fx.session();
    let entries = session.list_dir(".").unwrap();
    let names: Vec<(&str, Kind)> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(names, [("b.txt", Kind::File), ("src", Kind::Dir)]);
    let denied = session.list_dir(fx.home.to_str().unwrap()).unwrap_err();
    assert!(denied.contains("fs.read"), "{denied}");
    assert!(session.list_dir(r"\\host\share").is_err());
}
