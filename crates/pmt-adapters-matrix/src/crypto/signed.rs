//! Signed JSON (Matrix spec, Signing JSON): an object is signed over its
//! canonical form (keys sorted, no insignificant whitespace) without its
//! `signatures` and `unsigned` members; the signature goes under
//! `signatures.<user>.<algorithm:key id>`.

use serde_json::{Map, Value, json};
use vodozemac::{Ed25519PublicKey, Ed25519Signature};

/// `value` in canonical JSON. The workspace's `serde_json` keeps insertion
/// order, so keys are sorted here.
pub(crate) fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let members: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", members.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// The bytes a signature of `object` covers.
pub(crate) fn signable(object: &Value) -> String {
    let mut copy = object.clone();
    if let Some(map) = copy.as_object_mut() {
        map.remove("signatures");
        map.remove("unsigned");
    }
    canonical(&copy)
}

/// Adds `signature` by `user`'s key `key_id` (`ed25519:<id>`) to `object`.
pub(crate) fn add_signature(object: &mut Value, user: &str, key_id: &str, signature: &str) {
    if !object["signatures"].is_object() {
        object["signatures"] = json!({});
    }
    if !object["signatures"][user].is_object() {
        object["signatures"][user] = Value::Object(Map::new());
    }
    object["signatures"][user][key_id] = json!(signature);
}

/// Whether `object` carries a valid signature by `user`'s `key_id` made
/// with `key`.
pub(crate) fn verify(object: &Value, user: &str, key_id: &str, key: &Ed25519PublicKey) -> bool {
    let Some(signature) = object["signatures"][user][key_id].as_str() else {
        return false;
    };
    let Ok(signature) = Ed25519Signature::from_base64(signature) else {
        return false;
    };
    key.verify(signable(object).as_bytes(), &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_drops_whitespace() {
        let value = json!({"b": [1, {"z": true, "a": null}], "a": "\u{e9}", "c": {"y": 1, "x": 2}});
        assert_eq!(
            canonical(&value),
            r#"{"a":"é","b":[1,{"a":null,"z":true}],"c":{"x":2,"y":1}}"#
        );
    }

    #[test]
    fn a_signature_covers_everything_but_signatures_and_unsigned() {
        let account = vodozemac::olm::Account::new();
        let mut object = json!({"user_id": "@bot:hs", "keys": {"k": "v"}});
        let signature = account.sign(signable(&object)).to_base64();
        add_signature(&mut object, "@bot:hs", "ed25519:DEV", &signature);
        object["unsigned"] = json!({"device_display_name": "x"});
        let key = account.ed25519_key();
        assert!(verify(&object, "@bot:hs", "ed25519:DEV", &key));
        object["keys"]["k"] = json!("changed");
        assert!(!verify(&object, "@bot:hs", "ed25519:DEV", &key));
    }
}
