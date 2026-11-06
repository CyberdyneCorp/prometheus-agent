//! Delegation ids: `sa-<ULID>` for a child, `dg-<ULID>` for a spawn call.

/// A fresh child id: `sa-` and a 26-character ULID.
#[must_use]
pub fn subagent_id() -> String {
    format!("sa-{}", ulid::Ulid::generate())
}

/// A fresh delegation id: `dg-` and a 26-character ULID.
#[must_use]
pub fn delegation_id() -> String {
    format!("dg-{}", ulid::Ulid::generate())
}

/// Whether `id` has the shape of a child id.
#[must_use]
pub fn is_subagent_id(id: &str) -> bool {
    id.strip_prefix("sa-")
        .is_some_and(|ulid| ulid.len() == 26 && ulid::Ulid::from_string(ulid).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_carry_their_prefix_and_a_ulid() {
        let id = subagent_id();
        assert!(is_subagent_id(&id), "{id}");
        assert_eq!(id.len(), 29);
        assert!(delegation_id().starts_with("dg-"));
        assert!(!is_subagent_id("sa-nope"));
    }
}
