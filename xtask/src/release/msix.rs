//! The Windows packaging files of a release (spec: distribution-and-update,
//! Desktop packages and signing): the MSIX manifests of the App Installer
//! identity and of the Microsoft Store identity, per architecture, and the
//! `.appinstaller` file that checks for updates on every launch. The
//! release workflow packs and signs them on Windows (makeappx, signtool).
//!
//! Both identities run the same two executables: the desktop app and the
//! bundled `prometheus.exe`, which the execution aliases `prometheus.exe`
//! and `pmt.exe` put on PATH. The receipt beside it says who updates the
//! install (`msix` or `msstore`), so the CLI never updates itself there.

use std::path::Path;

use pmt_update::{Channel, ReleaseVersion};

/// The App Installer identity.
pub(super) const DIRECT_IDENTITY: &str = "CyberdyneCorp.Prometheus";
/// The Microsoft Store identity (a separate package family).
pub(super) const STORE_IDENTITY: &str = "CyberdyneCorp.PrometheusStore";
/// Windows 10 1809, the oldest Windows the packages install on.
const MIN_WINDOWS: &str = "10.0.17763.0";
/// What MSIX names the architectures the release builds.
const ARCHES: [&str; 2] = ["x64", "arm64"];
/// The first hour of canary numbering (see [`msix_version`]).
const CANARY_EPOCH: &str = "2026-01-01T00:00:00Z";

/// Writes the manifests, the App Installer file, `version.txt` (the MSIX
/// version) and `publisher.txt` (the App Installer identity's publisher)
/// into `out`.
pub(super) fn write(
    tag: &str,
    publisher: &str,
    store_publisher: &str,
    out: &Path,
) -> Result<(), String> {
    let version = msix_version(tag)?;
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut files = vec![
        ("version.txt".to_owned(), version.clone()),
        ("publisher.txt".to_owned(), publisher.to_owned()),
        (
            "prometheus-desktop.appinstaller".to_owned(),
            appinstaller(tag, &version, publisher)?,
        ),
    ];
    for arch in ARCHES {
        let direct = Identity {
            name: DIRECT_IDENTITY,
            display: "Prometheus",
            publisher,
        };
        let store = Identity {
            name: STORE_IDENTITY,
            display: "Prometheus",
            publisher: store_publisher,
        };
        files.push((
            format!("AppxManifest-direct-{arch}.xml"),
            manifest(&direct, &version, arch),
        ));
        files.push((
            format!("AppxManifest-store-{arch}.xml"),
            manifest(&store, &version, arch),
        ));
    }
    for (name, text) in files {
        let path = out.join(name);
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// The MSIX version of release `tag`: four 16-bit numbers that order like
/// the releases. A stable `vX.Y.Z` is `X.Y.Z.0`. A canary comes before its
/// stable, so `vX.Y.Z-canary.<ts>` sits just above the previous patch:
/// `X.Y.(Z-1).<n>`, where `n` (from 1) counts hours since 2026; with Z = 0
/// it borrows from the minor (`X.(Y-1).65535.<n>`), then the major.
pub(super) fn msix_version(tag: &str) -> Result<String, String> {
    let parsed = ReleaseVersion::parse(tag).map_err(|e| e.to_string())?;
    let text = tag.trim_start_matches('v');
    let (numbers, canary) = text
        .split_once("-canary.")
        .map_or((text, None), |(n, ts)| (n, Some(ts)));
    let mut parts = [0_u64; 3];
    for (slot, part) in parts.iter_mut().zip(numbers.split('.')) {
        *slot = part
            .parse()
            .map_err(|_| format!("{tag}: not a release version"))?;
    }
    let [major, minor, patch] = parts;
    let fields = match (parsed.channel(), canary) {
        (Channel::Canary, Some(ts)) => {
            let n = canary_hours(ts)? + 1;
            match (major, minor, patch) {
                (_, _, p) if p > 0 => [major, minor, p - 1, n],
                (_, m, 0) if m > 0 => [major, m - 1, 65535, n],
                (x, 0, 0) if x > 0 => [x - 1, 65535, 65535, n],
                _ => [0, 0, 0, n],
            }
        }
        _ => [major, minor, patch, 0],
    };
    if let Some(big) = fields.iter().find(|f| **f > 65535) {
        return Err(format!(
            "{tag}: MSIX version fields are 16-bit, {big} does not fit"
        ));
    }
    Ok(fields.map(|f| f.to_string()).join("."))
}

/// Whole hours from [`CANARY_EPOCH`] to the canary timestamp
/// `YYYYMMDDHHMMSS` (UTC).
fn canary_hours(ts: &str) -> Result<u64, String> {
    let at = jiff::civil::DateTime::strptime("%Y%m%d%H%M%S", ts)
        .and_then(|dt| dt.to_zoned(jiff::tz::TimeZone::UTC))
        .map_err(|e| format!("canary timestamp {ts}: {e}"))?;
    let epoch: jiff::Timestamp = CANARY_EPOCH.parse().map_err(|e| format!("{e}"))?;
    let seconds = at.timestamp().as_second() - epoch.as_second();
    u64::try_from(seconds / 3600).map_err(|_| format!("canary timestamp {ts} is before 2026"))
}

/// An MSIX package identity.
struct Identity<'a> {
    name: &'a str,
    display: &'a str,
    publisher: &'a str,
}

/// XML text escaping for attribute values.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The `AppxManifest.xml` of `identity` for `arch`: the desktop app (full
/// trust) and the console CLI with its execution aliases.
fn manifest(identity: &Identity<'_>, version: &str, arch: &str) -> String {
    let name = identity.name;
    let display = escape(identity.display);
    let publisher = escape(identity.publisher);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Package
  xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
  xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
  xmlns:uap3="http://schemas.microsoft.com/appx/manifest/uap/windows10/3"
  xmlns:desktop="http://schemas.microsoft.com/appx/manifest/desktop/windows10"
  xmlns:desktop4="http://schemas.microsoft.com/appx/manifest/desktop/windows10/4"
  xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
  IgnorableNamespaces="uap uap3 desktop desktop4 rescap">
  <Identity Name="{name}" Publisher="{publisher}" Version="{version}" ProcessorArchitecture="{arch}" />
  <Properties>
    <DisplayName>{display}</DisplayName>
    <PublisherDisplayName>CyberdyneCorp</PublisherDisplayName>
    <Logo>Assets\StoreLogo.png</Logo>
  </Properties>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="{MIN_WINDOWS}" MaxVersionTested="10.0.26100.0" />
  </Dependencies>
  <Resources>
    <Resource Language="en-us" />
  </Resources>
  <Applications>
    <Application Id="Prometheus" Executable="prometheus-desktop.exe" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements DisplayName="{display}" Description="The Prometheus agent" BackgroundColor="transparent"
        Square150x150Logo="Assets\Square150x150Logo.png" Square44x44Logo="Assets\Square44x44Logo.png" />
      <Extensions>
        <uap3:Extension Category="windows.protocol">
          <uap3:Protocol Name="prometheus" Parameters="&quot;%1&quot;">
            <uap:DisplayName>Prometheus</uap:DisplayName>
          </uap3:Protocol>
        </uap3:Extension>
      </Extensions>
    </Application>
    <Application Id="PrometheusCli" Executable="prometheus.exe" EntryPoint="Windows.FullTrustApplication"
      desktop4:Subsystem="console" desktop4:SupportsMultipleInstances="true">
      <uap:VisualElements DisplayName="Prometheus CLI" Description="The prometheus command" BackgroundColor="transparent"
        Square150x150Logo="Assets\Square150x150Logo.png" Square44x44Logo="Assets\Square44x44Logo.png" AppListEntry="none" />
      <Extensions>
        <uap3:Extension Category="windows.appExecutionAlias" EntryPoint="Windows.FullTrustApplication" Executable="prometheus.exe">
          <uap3:AppExecutionAlias>
            <desktop:ExecutionAlias Alias="prometheus.exe" />
            <desktop:ExecutionAlias Alias="pmt.exe" />
          </uap3:AppExecutionAlias>
        </uap3:Extension>
      </Extensions>
    </Application>
  </Applications>
  <Capabilities>
    <rescap:Capability Name="runFullTrust" />
  </Capabilities>
</Package>
"#
    )
}

/// The App Installer file: hosted at
/// `<origin>/channels/desktop-<channel>.appinstaller`, it points at this
/// release's bundle and has Windows check for a newer one on every launch.
fn appinstaller(tag: &str, version: &str, publisher: &str) -> Result<String, String> {
    let channel = ReleaseVersion::parse(tag)
        .map_err(|e| e.to_string())?
        .channel();
    let origin = pmt_update::DEFAULT_ORIGIN;
    let publisher = escape(publisher);
    let bundle = super::desktop::WINDOWS_BUNDLE;
    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<AppInstaller xmlns="http://schemas.microsoft.com/appx/appinstaller/2018"
  Uri="{origin}/channels/desktop-{channel}.appinstaller" Version="{version}">
  <MainBundle Name="{DIRECT_IDENTITY}" Publisher="{publisher}" Version="{version}"
    Uri="{origin}/{tag}/{bundle}" />
  <UpdateSettings>
    <OnLaunch HoursBetweenUpdateChecks="0" />
  </UpdateSettings>
</AppInstaller>
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::{msix_version, write};

    #[test]
    fn msix_versions_order_like_the_releases() {
        assert_eq!(msix_version("v1.6.0").unwrap(), "1.6.0.0");
        // 2026-11-13 12:30 is hour 7596 of 2026.
        assert_eq!(
            msix_version("v1.6.3-canary.20261113123000").unwrap(),
            "1.6.2.7597"
        );
        assert_eq!(
            msix_version("v1.6.0-canary.20260101000000").unwrap(),
            "1.5.65535.1"
        );
        assert_eq!(
            msix_version("v2.0.0-canary.20260101010000").unwrap(),
            "1.65535.65535.2"
        );
        let key = |v: &str| {
            msix_version(v)
                .unwrap()
                .split('.')
                .map(|p| p.parse::<u32>().unwrap())
                .collect::<Vec<_>>()
        };
        let ordered = [
            "v1.5.9",
            "v1.6.0-canary.20260301000000",
            "v1.6.0-canary.20260302000000",
            "v1.6.0",
            "v1.6.1-canary.20260401000000",
            "v1.6.1",
        ];
        for pair in ordered.windows(2) {
            assert!(key(pair[0]) < key(pair[1]), "{pair:?}");
        }
        assert!(msix_version("v70000.0.0").is_err(), "16-bit fields");
        assert!(msix_version("v1.0.0-canary.20251231230000").is_err());
    }

    #[test]
    fn the_packages_put_the_cli_on_path_and_check_for_updates_on_launch() {
        let dir = crate::repo_root().join("target/xtask-test/msix");
        let _ = std::fs::remove_dir_all(&dir);
        write("v1.6.0", "CN=Cyberdyne & Co", "CN=STORE-ID", &dir).unwrap();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        let direct = read("AppxManifest-direct-arm64.xml");
        for wanted in [
            r#"Name="CyberdyneCorp.Prometheus" Publisher="CN=Cyberdyne &amp; Co" Version="1.6.0.0" ProcessorArchitecture="arm64""#,
            r#"<desktop:ExecutionAlias Alias="prometheus.exe" />"#,
            r#"<desktop:ExecutionAlias Alias="pmt.exe" />"#,
            r#"MinVersion="10.0.17763.0""#,
            r#"desktop4:Subsystem="console""#,
        ] {
            assert!(direct.contains(wanted), "{wanted}\n{direct}");
        }
        let store = read("AppxManifest-store-x64.xml");
        assert!(
            store.contains(r#"Name="CyberdyneCorp.PrometheusStore" Publisher="CN=STORE-ID""#),
            "{store}"
        );
        let installer = read("prometheus-desktop.appinstaller");
        for wanted in [
            "/channels/desktop-stable.appinstaller",
            "/v1.6.0/prometheus-desktop-x86_64-pc-windows-msvc.msixbundle",
            r#"<OnLaunch HoursBetweenUpdateChecks="0" />"#,
        ] {
            assert!(installer.contains(wanted), "{wanted}\n{installer}");
        }
        assert_eq!(read("version.txt"), "1.6.0.0");
        assert_eq!(read("publisher.txt"), "CN=Cyberdyne & Co");
    }
}
