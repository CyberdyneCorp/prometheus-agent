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

    /// The file offsets of every Mach-O slice (one for a thin binary).
    fn slices(bytes: &[u8]) -> Vec<usize> {
        let be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if be(0) != 0xcafe_babe {
            return vec![0];
        }
        (0..be(4)).map(|i| be(8 + i * 20 + 8)).collect()
    }

    /// The file range of the `__TEXT,__text` section of the 64-bit Mach-O
    /// slice at `slice`.
    fn text_section(bytes: &[u8], slice: usize) -> (usize, usize) {
        let le32 = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let le64 = |at: usize| {
            usize::try_from(u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())).unwrap()
        };
        let name = |at: usize| {
            bytes[at..at + 16]
                .split(|&b| b == 0)
                .next()
                .unwrap()
                .to_vec()
        };
        assert_eq!(le32(slice), 0xfeed_facf, "a 64-bit Mach-O slice");
        let mut command = slice + 32;
        for _ in 0..le32(slice + 16) {
            if le32(command) == 0x19 && name(command + 8) == b"__TEXT" {
                for i in 0..le32(command + 64) {
                    let section = command + 72 + i * 80;
                    if name(section) == b"__text" {
                        return (slice + le32(section + 48), le64(section + 40));
                    }
                }
            }
            command += le32(command + 4);
        }
        panic!("no __TEXT,__text section");
    }

    /// Flips one byte of machine code in every slice of the executable:
    /// `__text` pages are always hashed by the signature, whatever the
    /// host's copy of the binary looks like (the middle of a slice may be
    /// the unhashed signature blob itself).
    fn tamper(app: &Path) {
        let exe = app.join("Contents/MacOS/cua-driver");
        let mut bytes = std::fs::read(&exe).unwrap();
        for slice in slices(&bytes) {
            let (offset, size) = text_section(&bytes, slice);
            bytes[offset + size / 2] ^= 0xff;
        }
        std::fs::write(&exe, bytes).unwrap();
        let verdict = Command::new(CODESIGN)
            .args(["--verify", "--strict", "--deep"])
            .arg(app)
            .output()
            .unwrap();
        assert!(
            !verdict.status.success(),
            "codesign accepted the tampered bundle"
        );
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
