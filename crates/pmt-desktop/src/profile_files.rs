//! Profile files from the desktop (spec: desktop-app, Fonts, shortcuts,
//! palette and profile files): export and import run the bundled
//! binary's `profile export` and `profile import`, so the archive is the
//! `configuration-and-profiles` profile export format. The desktop never
//! asks for `--with-secrets`: an exported archive holds no secret values.
//! The desktop appearance travels as the archive's optional overlay and
//! comes back from an import for the page to apply.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::request::Request;
use crate::shell::Shell;

const BOUND: Duration = Duration::from_mins(5);

/// The overlay file an import leaves in the new profile.
const APPEARANCE_FILE: &str = "desktop-appearance.json";

pub(crate) fn owns(request: &Request) -> bool {
    matches!(
        request,
        Request::ProfileExport { .. } | Request::ProfileImport
    )
}

fn arg(path: &Path) -> String {
    path.display().to_string()
}

impl Shell {
    pub(crate) async fn profile_file(&self, request: Request) -> Result<Value, String> {
        match request {
            Request::ProfileExport {
                profile,
                appearance,
            } => self.export_profile(&profile, appearance.as_ref()).await,
            _ => self.import_profile().await,
        }
    }

    async fn export_profile(
        &self,
        profile: &str,
        appearance: Option<&Value>,
    ) -> Result<Value, String> {
        let Some(output) = self.native.save_path(&format!("{profile}.tar.zst")) else {
            return Ok(json!({"cancelled": true}));
        };
        let overlay = match appearance {
            Some(value) => {
                let path = self
                    .config
                    .home
                    .join("run")
                    .join(format!("appearance-{}.json", std::process::id()));
                std::fs::write(&path, value.to_string())
                    .map_err(|e| format!("cannot stage the appearance: {e}"))?;
                Some(path)
            }
            None => None,
        };
        let mut args = vec![
            "profile".to_owned(),
            "export".to_owned(),
            profile.to_owned(),
            "-o".to_owned(),
            arg(&output),
        ];
        if let Some(path) = &overlay {
            args.extend(["--appearance".to_owned(), arg(path)]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = self.run_bundled(&args, BOUND).await;
        if let Some(path) = overlay {
            let _ = std::fs::remove_file(path);
        }
        ran?;
        Ok(json!({"path": arg(&output)}))
    }

    async fn import_profile(&self) -> Result<Value, String> {
        let Some(file) = self.native.pick(false, Some("Import a profile")) else {
            return Ok(json!({"cancelled": true}));
        };
        let manifest = self
            .bundled_json(&["profile", "import", &arg(&file)], BOUND)
            .await?;
        let profile = manifest["profile"].as_str().unwrap_or_default().to_owned();
        let overlay: PathBuf = self
            .config
            .home
            .join("profiles")
            .join(&profile)
            .join(APPEARANCE_FILE);
        let appearance = std::fs::read_to_string(&overlay)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        Ok(json!({"profile": profile, "appearance": appearance}))
    }
}
