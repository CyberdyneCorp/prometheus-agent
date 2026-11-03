//! Matrix power levels as roles (spec: messaging-gateway, Role-based
//! access grants). A Matrix room has no named roles: a member has a power
//! level (`m.room.power_levels`, default `users_default`), and a level
//! grants everything a lower level may do. So a role id is a level, and a
//! member holds every level from 0 up to their own: `allow_roles = ["50"]`
//! admits moderators (50) and admins (100) alike. Levels above 100 are
//! rare and held exactly.

use pmt_gateway::messaging::SendError;
use serde_json::Value;

use crate::api::{Api, segment};

/// The highest level held as a range (the conventional admin level).
const RANGE_TOP: i64 = 100;

/// `user`'s power level in the room's power-levels content.
fn level(content: &Value, user: &str) -> i64 {
    content["users"][user]
        .as_i64()
        .or_else(|| content["users_default"].as_i64())
        .unwrap_or(0)
}

/// The role ids a member at `level` holds.
fn ranks(level: i64) -> Vec<String> {
    let mut held: Vec<String> = (0..=level.min(RANGE_TOP)).map(|l| l.to_string()).collect();
    if !(0..=RANGE_TOP).contains(&level) {
        held.push(level.to_string());
    }
    held
}

/// The levels `user` holds in `room`; an error when the room's power
/// levels cannot be read (the gateway then denies the grant).
pub(crate) async fn held(api: &Api, room: &str, user: &str) -> Result<Vec<String>, SendError> {
    let content = api
        .get(&format!(
            "/rooms/{}/state/m.room.power_levels",
            segment(room)
        ))
        .await?;
    Ok(ranks(level(&content, user)))
}

/// The level a conventional role name stands for.
pub(crate) fn named(name: &str) -> Option<String> {
    let level = match name.trim().to_ascii_lowercase().as_str() {
        "admin" | "admins" | "administrator" | "administrators" => 100,
        "moderator" | "moderators" | "mod" | "mods" => 50,
        "user" | "users" | "member" | "members" | "default" => 0,
        _ => return None,
    };
    Some(level.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_member_holds_every_level_up_to_their_own() {
        let content = json!({"users": {"@mod:x": 50, "@admin:x": 100}, "users_default": 0});
        let moderator = ranks(level(&content, "@mod:x"));
        assert!(moderator.contains(&"50".to_owned()) && moderator.contains(&"0".to_owned()));
        assert!(!moderator.contains(&"100".to_owned()));
        assert!(ranks(level(&content, "@admin:x")).contains(&"50".to_owned()));
        assert_eq!(ranks(level(&content, "@someone:x")), ["0"]);
        assert!(ranks(150).contains(&"150".to_owned()));
    }

    #[test]
    fn conventional_names_resolve_to_levels() {
        assert_eq!(named("Moderators").as_deref(), Some("50"));
        assert_eq!(named("admin").as_deref(), Some("100"));
        assert_eq!(named("ops"), None);
    }
}
