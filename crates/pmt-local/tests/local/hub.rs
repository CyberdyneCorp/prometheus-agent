//! Hugging Face search and community pulls (spec: local-models, Model
//! library) against a fixture hub.

use pmt_config::daemon_local::LocalBackend;
use pmt_local::backend;
use pmt_local::catalog::Fit;
use pmt_local::download::Jobs;
use pmt_local::hardware::{GIB, Gpu, Hardware, Vendor};
use pmt_local::hub::Hub;
use pmt_local::library::{Library, Role};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{client, temp};

fn machine() -> Hardware {
    Hardware {
        platform: "linux-x64".to_owned(),
        ram_total: 32 * GIB,
        ram_available: 32 * GIB,
        gpus: vec![Gpu {
            vendor: Vendor::Nvidia,
            name: "NVIDIA GeForce RTX 4060".to_owned(),
            memory_total: 8 * GIB,
            memory_used: 0,
            integrated: false,
            cuda: Some(13),
            bandwidth_gbps: None,
        }],
    }
}

fn file(path: &str, size: u64) -> serde_json::Value {
    json!({"type": "file", "path": path, "size": size, "lfs": {"oid": "a".repeat(64), "size": size}})
}

async fn hub() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/models"))
        .and(query_param("search", "qwen"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "someone/Qwen-X-GGUF", "downloads": 1234}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/models/someone/Qwen-X-GGUF/tree/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            file("Qwen-X-Q8_0.gguf", 9 * GIB),
            file("Qwen-X-Q4_K_M.gguf", 5 * GIB),
            file("Qwen-X-IQ2_XXS.gguf", 2 * GIB),
            file("mmproj-Qwen-X-F16.gguf", GIB / 2),
            {"type": "file", "path": "README.md", "size": 10},
        ])))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn search_lists_community_files_of_4_bits_or_more_with_their_fit() {
    let server = hub().await;
    let selection = backend::select(LocalBackend::Auto, &machine());
    let found = Hub::new(&server.uri(), client())
        .search("qwen", &selection)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "someone/Qwen-X-GGUF");
    assert_eq!(found[0].downloads, 1234);
    assert!(!found[0].validated, "community models carry no badge");
    let files: Vec<(&str, Fit)> = found[0]
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.fit))
        .collect();
    assert_eq!(
        files,
        [
            ("Qwen-X-Q8_0.gguf", Fit::UsesSystemRam),
            ("Qwen-X-Q4_K_M.gguf", Fit::FitsGpu)
        ]
    );
}

#[tokio::test]
async fn a_community_pull_picks_the_best_file_that_fits_and_its_projector() {
    let server = hub().await;
    let selection = backend::select(LocalBackend::Auto, &machine());
    let home = temp();
    let (library, jobs) = (Library::open(home.path()), Jobs::open(home.path()));
    let job = Hub::new(&server.uri(), client())
        .job("someone/Qwen-X-GGUF", &selection, &library, &jobs)
        .await
        .unwrap();
    assert_eq!(job.subject, "qwen-x");
    let names: Vec<&str> = job.files.iter().map(|f| f.spec.name.as_str()).collect();
    assert_eq!(names, ["Qwen-X-Q4_K_M.gguf", "mmproj-Qwen-X-F16.gguf"]);
    assert!(
        job.files[0]
            .spec
            .url
            .ends_with("/someone/Qwen-X-GGUF/resolve/main/Qwen-X-Q4_K_M.gguf")
    );
    let model: pmt_local::library::Model = serde_json::from_value(job.meta.clone()).unwrap();
    assert_eq!(model.files[1].role, Role::Projector);
    assert_eq!(library.list(&jobs)[0].state, "partial");
    let other = temp();
    let below_4_bits = Hub::new(&server.uri(), client())
        .job(
            "someone/Qwen-X-GGUF:IQ2_XXS",
            &selection,
            &library,
            &Jobs::open(other.path()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        below_4_bits.exit_code(),
        66,
        "no build below 4 bits is offered"
    );
}
