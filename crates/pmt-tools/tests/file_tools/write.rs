//! `write_file`: stale-overwrite protection, write guards and post-write
//! diagnostics.

use std::fs;

use pmt_tools::files::{Linter, ReadArgs};
use pmt_tools::result::Status;

use crate::support::{Fixture, body, read_all};

#[test]
fn an_edit_after_an_out_of_band_change_is_refused() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    let session = fx.session();
    read_all(&session, "a.rs");
    // Another process edits the file after the session read it.
    fx.write("a.rs", "fn external() {}\n");
    let result = session.write_file("a.rs", "fn mine() {}\n");
    assert_eq!(result.status, Status::Error);
    let reply = body(&result);
    assert_eq!(reply["stale_write_blocked"], true);
    assert_eq!(reply["path"], "a.rs");
    assert_eq!(fx.read("a.rs"), "fn external() {}\n");
    // Reading it again makes the write safe.
    read_all(&session, "a.rs");
    assert_eq!(
        session.write_file("a.rs", "fn mine() {}\n").status,
        Status::Ok
    );
    assert_eq!(fx.read("a.rs"), "fn mine() {}\n");
}

#[test]
fn unread_or_partly_read_files_are_not_overwritten_but_new_files_are_written() {
    let fx = Fixture::new();
    fx.write("long.txt", "x\n".repeat(30));
    let session = fx.session();
    let unread = session.write_file("long.txt", "replaced\n");
    assert_eq!(body(&unread)["stale_write_blocked"], true);
    let _ = session.read_file(&ReadArgs {
        limit: 10,
        ..ReadArgs::new("long.txt")
    });
    assert_eq!(
        body(&session.write_file("long.txt", "y\n"))["stale_write_blocked"],
        true
    );
    assert_eq!(fx.read("long.txt"), "x\n".repeat(30));

    let created = session.write_file("new/dir/b.rs", "fn b() {}\n");
    assert_eq!(created.status, Status::Ok, "{}", created.text_content());
    assert_eq!(body(&created)["created"], true);
    // A file the session wrote may be written again without a read.
    assert_eq!(
        session.write_file("new/dir/b.rs", "fn c() {}\n").status,
        Status::Ok
    );
    assert_eq!(fx.read("new/dir/b.rs"), "fn c() {}\n");
}

#[test]
fn a_secret_file_can_be_overwritten_without_a_read_but_not_after_an_external_change() {
    let fx = Fixture::new();
    fx.write(".env", "TOKEN=old\n");
    let session = fx.session();
    // The session can never read a dotenv file, so the full-read rule
    // cannot apply to it; writing it is explicitly allowed.
    let first = session.write_file(".env", "TOKEN=new\n");
    assert_eq!(first.status, Status::Ok, "{}", first.text_content());
    assert_eq!(fx.read(".env"), "TOKEN=new\n");
    // Changed on disk since the session's own write: still blocked.
    fx.write(".env", "TOKEN=user\n");
    let blocked = session.write_file(".env", "TOKEN=mine\n");
    assert_eq!(body(&blocked)["stale_write_blocked"], true);
    assert_eq!(fx.read(".env"), "TOKEN=user\n");
}

#[test]
fn control_files_of_every_profile_are_refused() {
    let fx = Fixture::new();
    let session = fx.session();
    let grants = fx.prometheus.join("profiles/default/grants.toml");
    let result = session.write_file(grants.to_str().unwrap(), "[approvals]\nallow = [\"*\"]\n");
    assert_eq!(result.status, Status::Denied);
    assert_eq!(
        result.text_content(),
        "refusing to write Prometheus control file: grants.toml"
    );
    assert_eq!(fs::read_to_string(&grants).unwrap(), "# control\n");
    let other = fx.prometheus.join("profiles/work/config.toml");
    let refused = session.write_file(other.to_str().unwrap(), "x = 1\n");
    assert_eq!(refused.status, Status::Denied);
    assert!(!other.exists());
}

#[cfg(unix)]
#[test]
fn a_link_into_the_prometheus_home_is_refused_too() {
    let fx = Fixture::new();
    let secrets = fx.prometheus.join("profiles/default/secrets.toml");
    std::os::unix::fs::symlink(&secrets, fx.path("innocent.toml")).unwrap();
    let result = fx.session().write_file("innocent.toml", "X = \"y\"\n");
    assert_eq!(
        result.text_content(),
        "refusing to write Prometheus control file: secrets.toml"
    );
    assert_eq!(fs::read_to_string(&secrets).unwrap(), "# control\n");
}

#[cfg(unix)]
#[test]
fn system_paths_and_binary_files_are_refused() {
    let fx = Fixture::new();
    let session = fx.session_with(Linter::builtin(), &["fs.write:/etc/**"]);
    let result = session.write_file("/etc/hosts", "127.0.0.1 evil\n");
    assert_eq!(result.status, Status::Denied);
    assert_eq!(
        result.text_content(),
        "refusing to write system path: /etc/hosts"
    );

    fx.write("logo.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
    let result = fx.session().write_file("logo.png", "text");
    assert_eq!(
        result.text_content(),
        "refusing to overwrite binary file 'logo.png' with text"
    );
    assert_eq!(fs::read(fx.path("logo.png")).unwrap()[0], 0x89);
}

#[test]
fn a_syntax_error_is_reported_and_the_file_is_still_written() {
    let fx = Fixture::new();
    let result = fx
        .session()
        .write_file("settings.json", "{\n  \"a\": 1,\n}\n");
    assert_eq!(result.status, Status::Ok);
    let reply = body(&result);
    assert_eq!(reply["lint"]["status"], "error");
    let output = reply["lint"]["output"].as_str().unwrap();
    assert!(
        output.contains("trailing comma") && output.contains("line 3"),
        "{output}"
    );
    assert_eq!(fx.read("settings.json"), "{\n  \"a\": 1,\n}\n");
    let clean = body(&fx.session().write_file("ok.json", "{\"a\": 1}"));
    assert_eq!(clean["lint"]["status"], "ok");
}

#[test]
fn a_crashing_checker_degrades_to_skipped() {
    let fx = Fixture::new();
    let linter = Linter::builtin().with("yaml", |_| panic!("YAML checker bug"));
    let result = fx
        .session_with(linter, &[])
        .write_file("ci.yaml", "on: push\n");
    assert_eq!(result.status, Status::Ok);
    assert_eq!(body(&result)["lint"]["status"], "skipped");
    assert_eq!(fx.read("ci.yaml"), "on: push\n");
}
