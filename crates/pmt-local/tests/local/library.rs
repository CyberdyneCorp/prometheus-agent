//! The machine-wide model library (spec: local-models, Model library).

use pmt_local::download::Jobs;
use pmt_local::library::{Library, Source};

use crate::support::temp;

#[test]
fn removing_a_linked_model_leaves_the_users_file_in_place() {
    let home = temp();
    let user = temp();
    let original = user.path().join("x.gguf");
    std::fs::write(
        &original,
        pmt_local::gguf::fixture("llama", 32_768, 4, 2, 64),
    )
    .unwrap();
    let library = Library::open(home.path());
    let jobs = Jobs::open(home.path());

    let model = library
        .link(&original, |info| {
            pmt_local::pull::start_window(info.context_length)
        })
        .unwrap();
    assert_eq!(model.id, "x");
    assert_eq!(model.source, Source::Linked);
    assert_eq!(
        model.context, 32_768,
        "the file's own window, below the minimum"
    );
    assert_eq!(
        model.weights(),
        Some(original.as_path()),
        "linked, not copied"
    );
    assert_eq!(library.list(&jobs)[0].state, "ready");

    library.remove("x", &jobs).unwrap();
    assert!(library.models().is_empty());
    assert!(original.is_file(), "the linked original still exists");
    assert_eq!(library.remove("x", &jobs).unwrap_err().exit_code(), 66);
}

#[test]
fn removing_a_staged_model_deletes_every_file_it_staged() {
    let home = temp();
    let library = Library::open(home.path());
    let jobs = Jobs::open(home.path());
    let dir = library.dir("qwen-x").unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let files = [
        "w-00001-of-00002.gguf",
        "w-00002-of-00002.gguf",
        "mmproj.gguf",
        "draft.gguf",
    ];
    for name in files {
        std::fs::write(dir.join(name), b"GGUF").unwrap();
    }
    let model: pmt_local::library::Model = serde_json::from_value(serde_json::json!({
        "id": "qwen-x", "source": "catalog", "context": 65536, "added_at": "",
        "files": [
            {"path": dir.join(files[0]), "role": "weights", "size": 4},
            {"path": dir.join(files[1]), "role": "part", "size": 4},
            {"path": dir.join(files[2]), "role": "projector", "size": 4},
            {"path": dir.join(files[3]), "role": "draft", "size": 4},
        ],
    }))
    .unwrap();
    library.publish(&model).unwrap();
    assert_eq!(library.model("qwen-x", &jobs).unwrap(), model);
    library.remove("qwen-x", &jobs).unwrap();
    assert!(!dir.exists());
}

#[test]
fn a_file_that_is_not_gguf_is_refused() {
    let home = temp();
    let path = home.path().join("notes.gguf");
    std::fs::write(&path, b"hello").unwrap();
    let err = Library::open(home.path())
        .link(&path, |_| 65_536)
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn ids_that_leave_the_library_are_refused_and_nothing_is_deleted() {
    let home = temp();
    let outside = temp();
    std::fs::write(outside.path().join("keep"), b"x").unwrap();
    let library = Library::open(home.path());
    let jobs = Jobs::open(home.path());
    std::fs::create_dir_all(library.root()).unwrap();
    let absolute = outside.path().to_string_lossy().into_owned();
    for id in ["..", ".", "", absolute.as_str(), "a/../..", "a\\b", "c:x"] {
        let err = library.remove(id, &jobs).unwrap_err();
        assert_eq!(err.exit_code(), 66, "{id}");
        assert!(library.dir(id).is_err(), "{id}");
    }
    assert!(outside.path().join("keep").is_file());
    assert!(library.root().is_dir());
}

#[test]
fn a_hub_file_name_that_traverses_is_refused() {
    let home = temp();
    let dir = Library::open(home.path()).dir("m").unwrap();
    for name in ["..", "a\\..\\..\\x.gguf", "C:x.gguf", "sub/..", ""] {
        assert!(
            pmt_local::pull::spec(String::new(), name, 1, "", &dir).is_err(),
            "{name}"
        );
    }
    assert!(pmt_local::pull::spec(String::new(), "sub/w.gguf", 1, "", &dir).is_ok());
}
