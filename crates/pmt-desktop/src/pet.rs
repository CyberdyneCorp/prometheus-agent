//! The desktop pet's windows (spec: pets, Desktop pet and pop-out
//! overlay). The pet pops out of the main window into a transparent,
//! always-on-top overlay window labelled `pet`, which has no protocol
//! connection: the two windows talk only through [`Request::PetRelay`]
//! (delivered as the `pet-message` event), so a prompt typed on the pet
//! goes out on the main window's connection. The overlay's place on the
//! screen is kept in `<home>/desktop/pet-overlay.json`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Request;
use crate::shell::Shell;

/// The overlay window's label.
pub const OVERLAY: &str = "pet";
/// The main window's label.
pub const MAIN: &str = "main";
/// The event a relayed message reaches a window with.
pub const MESSAGE_EVENT: &str = "pet-message";

/// What [`Request::MainWindow`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainAction {
    /// Hide it when it shows, else show it.
    Toggle,
    /// Show it.
    Show,
}

/// Whether the pet's requests own `request`.
#[must_use]
pub fn owns(request: &Request) -> bool {
    matches!(
        request,
        Request::PetOverlay { .. }
            | Request::PetRelay { .. }
            | Request::PetDrag
            | Request::MainWindow { .. }
    )
}

/// Where the overlay sits, in physical screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    /// Left.
    pub x: i32,
    /// Top.
    pub y: i32,
}

/// The file holding the overlay's place under `home`.
#[must_use]
pub fn place_file(home: &Path) -> PathBuf {
    home.join("desktop").join("pet-overlay.json")
}

/// The overlay's saved place.
#[must_use]
pub fn saved_place(home: &Path) -> Option<Place> {
    let text = std::fs::read_to_string(place_file(home)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Remembers the overlay's place (best effort: a failure leaves the
/// previous one).
pub fn save_place(home: &Path, place: Place) {
    let file = place_file(home);
    if let Some(dir) = file.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = std::fs::write(&file, json!(place).to_string());
    }
}

fn refuse(window: &str, what: &str) -> Result<Value, String> {
    Err(format!("window {window} may not {what}"))
}

impl Shell {
    /// Runs one pet request of `window`.
    pub(crate) fn pet(&self, window: &str, request: Request) -> Result<Value, String> {
        let native = &self.native;
        match request {
            Request::PetOverlay { open } if window == MAIN || window == OVERLAY => {
                native.pet_overlay(open).map(|shows| json!({"open": shows}))
            }
            Request::PetRelay { to, message }
                if (window == MAIN && to == OVERLAY) || (window == OVERLAY && to == MAIN) =>
            {
                native
                    .emit_to(&to, MESSAGE_EVENT, &message)
                    .map(|()| json!({"ok": true}))
            }
            Request::PetDrag if window == OVERLAY => {
                native.drag_window(OVERLAY).map(|()| json!({"ok": true}))
            }
            Request::MainWindow { action } if window == OVERLAY => native
                .main_window(action == MainAction::Toggle)
                .map(|shows| json!({"visible": shows})),
            _ => refuse(window, "use the pet's window commands"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_and_places_round_trip() {
        let relay: Request = serde_json::from_value(json!({
            "command": "pet_relay", "args": {"to": "main", "message": {"type": "submit", "text": "hi"}}
        }))
        .unwrap();
        assert!(owns(&relay));
        let main: Request =
            serde_json::from_value(json!({"command": "main_window", "args": {"action": "toggle"}}))
                .unwrap();
        assert_eq!(
            main,
            Request::MainWindow {
                action: MainAction::Toggle
            }
        );
        let home = tempfile::tempdir().unwrap();
        assert_eq!(saved_place(home.path()), None);
        save_place(home.path(), Place { x: 40, y: -12 });
        assert_eq!(saved_place(home.path()), Some(Place { x: 40, y: -12 }));
    }
}
