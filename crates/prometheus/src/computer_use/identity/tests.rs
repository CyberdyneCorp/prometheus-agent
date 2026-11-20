use super::{bundle, judge, requirement};

#[test]
fn only_the_official_identity_passes() {
    let official = "Executable=/x\nIdentifier=com.trycua.driver\nTeamIdentifier=4YEC26S9KF\n";
    assert_eq!(judge(official, false), Ok(()));
    let adhoc = "Identifier=com.trycua.driver\nSignature=adhoc\nTeamIdentifier=not set\n";
    assert!(judge(adhoc, false).unwrap_err().contains("team 'not set'"));
    assert_eq!(judge(adhoc, true), Ok(()));
    let impostor = "Identifier=com.trycua.driver.evil\nTeamIdentifier=4YEC26S9KF\n";
    assert!(judge(impostor, true).unwrap_err().contains("identifier"));
    let other_team = "Identifier=com.trycua.driver\nTeamIdentifier=ABCDE12345\n";
    assert!(
        judge(other_team, true)
            .unwrap_err()
            .contains("team 'ABCDE12345'")
    );
}

#[test]
fn the_requirement_names_the_identifier_and_every_team() {
    assert_eq!(
        requirement(),
        "anchor apple generic and identifier \"com.trycua.driver\" and (certificate leaf[subject.OU] = \"4YEC26S9KF\" or certificate leaf[subject.OU] = \"YCK386LBJ7\")"
    );
}

#[test]
fn a_bundle_is_found_only_around_its_binary() {
    let temp = tempfile::tempdir().unwrap();
    let exe = temp.path().join("CuaDriver.app/Contents/MacOS/cua-driver");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "").unwrap();
    let found = bundle(&exe).unwrap();
    assert!(found.ends_with("CuaDriver.app"));
    let loose = temp.path().join("cua-driver");
    std::fs::write(&loose, "").unwrap();
    assert_eq!(bundle(&loose), None);
}

/// The real `codesign` on real bundles.
#[cfg(target_os = "macos")]
mod signed {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::super::{CODESIGN, verify};

    /// `CuaDriver.app` around a copy of a system binary, unsigned.
    fn unsigned_bundle(dir: &Path) -> PathBuf {
        let app = dir.join("CuaDriver.app");
        let exe = app.join("Contents/MacOS/cua-driver");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::copy("/usr/bin/true", &exe).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
             <key>CFBundleIdentifier</key><string>com.trycua.driver</string>\
             <key>CFBundleExecutable</key><string>cua-driver</string></dict></plist>\n",
        )
        .unwrap();
        // The copy carries Apple's signature: drop it.
        let status = Command::new(CODESIGN)
            .arg("--remove-signature")
            .arg(&exe)
            .status()
            .unwrap();
        assert!(status.success());
        app
    }

    /// The bundle signed ad hoc as the official identifier.
    fn adhoc_bundle(dir: &Path) -> PathBuf {
        let app = unsigned_bundle(dir);
        let status = Command::new(CODESIGN)
            .args(["-f", "-s", "-", "--identifier", "com.trycua.driver"])
            .arg(&app)
            .status()
            .unwrap();
        assert!(status.success());
        app
    }

    /// Flips one byte in the middle of the executable's first slice (a
    /// page the signature hashes).
    fn tamper(app: &Path) {
        let exe = app.join("Contents/MacOS/cua-driver");
        let mut bytes = std::fs::read(&exe).unwrap();
        let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let (offset, size) = if word(0) == 0xcafe_babe {
            (word(16), word(20))
        } else {
            (0, bytes.len())
        };
        bytes[offset + size / 2] ^= 0xff;
        std::fs::write(&exe, bytes).unwrap();
    }

    /// Scenario: Unsigned bundle.
    #[test]
    fn an_unsigned_bundle_fails_the_signature_check() {
        let temp = tempfile::tempdir().unwrap();
        let app = unsigned_bundle(temp.path());
        let why = verify(&app, false).unwrap_err();
        assert!(why.contains("code signature check refused"), "{why}");
        assert!(why.contains("not signed"), "{why}");
        assert_eq!(verify(&app, true), Ok(()));
    }

    #[test]
    fn an_adhoc_bundle_passes_only_when_unsigned_drivers_are_allowed() {
        let temp = tempfile::tempdir().unwrap();
        let app = adhoc_bundle(temp.path());
        let why = verify(&app, false).unwrap_err();
        assert!(why.contains("team 'not set'"), "{why}");
        assert_eq!(verify(&app, true), Ok(()));
    }

    /// `codesign -dv` still reports the identifier of a modified bundle;
    /// only a verification notices the change.
    #[test]
    fn a_signed_bundle_with_a_modified_executable_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let app = adhoc_bundle(temp.path());
        tamper(&app);
        for allow_unsigned in [false, true] {
            let why = verify(&app, allow_unsigned).unwrap_err();
            assert!(why.contains("code signature check refused"), "{why}");
        }
        let why = verify(&app, true).unwrap_err();
        assert!(why.contains("its signature does not verify"), "{why}");
        assert!(why.contains("modified"), "{why}");
    }
}
