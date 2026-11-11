//! The error taxonomy's contracts: kinds, exit codes and the code table.

use std::collections::HashSet;

use super::{ERROR_CODES, Error, ErrorKind, codes};
use crate::exit::EXIT_CODES;

#[test]
fn every_kind_maps_to_exactly_one_documented_exit_code() {
    for kind in ErrorKind::ALL {
        let documented = EXIT_CODES
            .iter()
            .filter(|e| e.code == kind.exit_code())
            .count();
        assert_eq!(documented, 1, "{kind} -> {}", kind.exit_code());
    }
}

#[test]
fn kind_table_is_the_spec_table() {
    let expected = [
        ("usage", 2),
        ("invalid_input", 65),
        ("not_found", 66),
        ("config", 78),
        ("auth", 78),
        ("permission_denied", 77),
        ("budget_exceeded", 3),
        ("provider", 69),
        ("tool", 1),
        ("sandbox", 77),
        ("conflict", 75),
        ("unavailable", 69),
        ("protocol", 76),
        ("cancelled", 130),
        ("io", 74),
        ("internal", 70),
    ];
    let actual: Vec<_> = ErrorKind::ALL
        .iter()
        .map(|k| (k.as_str(), k.exit_code()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn serialized_kind_is_the_wire_name() {
    for kind in ErrorKind::ALL {
        let json = serde_json::to_value(kind).unwrap();
        assert_eq!(json, kind.as_str());
    }
}

#[test]
fn catalog_codes_are_unique_and_dotted() {
    let mut seen = HashSet::new();
    for entry in ERROR_CODES {
        assert!(seen.insert(entry.code), "duplicate code {}", entry.code);
        let parts: Vec<_> = entry.code.split('.').collect();
        let well_formed = parts.len() >= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        assert!(well_formed, "code {} is not dotted snake_case", entry.code);
    }
}

#[test]
fn json_document_carries_kind_code_message_and_hint() {
    let err = Error::new(&codes::REGISTRY_UNKNOWN_NAME, "unknown provider 'x'")
        .with_hint("available providers: a, b");
    let doc: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
    assert_eq!(
        doc,
        serde_json::json!({"error": {
            "kind": "config",
            "code": "registry.unknown_name",
            "message": "unknown provider 'x'",
            "hint": "available providers: a, b",
        }})
    );
    assert_eq!(err.exit_code(), 78);
}

#[test]
fn json_document_has_null_hint_when_absent() {
    let err = Error::new(&codes::TASK_PANICKED, "task 'x' panicked");
    let doc: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
    assert_eq!(doc["error"]["hint"], serde_json::Value::Null);
    assert_eq!(doc["error"]["kind"], "internal");
}

#[test]
fn an_io_error_names_the_path_and_keeps_the_io_error_as_source() {
    let err = Error::io_doing(
        &codes::HOME_IO,
        "cannot write",
        std::path::Path::new("/x/config.toml"),
        &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    );
    assert!(err.message().starts_with("cannot write /x/config.toml: "));
    let source = std::error::Error::source(&err)
        .and_then(|s| s.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind);
    assert_eq!(source, Some(std::io::ErrorKind::PermissionDenied));
}
