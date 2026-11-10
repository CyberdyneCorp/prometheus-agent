//! Resumable, verifiable downloads (spec: local-models, Resumable,
//! verifiable downloads) against a Range-capable fixture origin.

use pmt_local::download::{Control, Downloader, JobState, Jobs, Progress};
use pmt_local::library::{Library, Model, ModelFile, Role, Source};
use pmt_local::pull::{self, Pulled};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::support::{client, sha, temp};

const SIZE: usize = 1_000_000;

fn weights() -> Vec<u8> {
    sized(SIZE)
}

fn sized(size: usize) -> Vec<u8> {
    (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

/// A model job for one weights file served at `url`.
fn job(jobs: &Jobs, library: &Library, url: &str, data: &[u8]) -> pmt_local::download::Job {
    let spec = pull::spec(
        url.to_owned(),
        "qwen-x-Q4_K_M.gguf",
        data.len() as u64,
        &sha(data),
        &library.dir("qwen-x").unwrap(),
    )
    .unwrap();
    let model = Model {
        id: "qwen-x".to_owned(),
        source: Source::Catalog,
        files: vec![ModelFile {
            path: spec.target.clone(),
            role: Role::Weights,
            size: spec.size,
        }],
        quant: Some("Q4_K_M".to_owned()),
        context: 65_536,
        context_max: Some(131_072),
        added_at: String::new(),
    };
    pull::create(jobs, &model, vec![spec]).unwrap()
}

/// Serves `data`, honouring `Range: bytes=<n>-` with 206.
struct Ranged(Vec<u8>);

impl Respond for Ranged {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let offset = request
            .headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.trim_end_matches('-').parse::<usize>().ok());
        match offset {
            Some(n) => ResponseTemplate::new(206).set_body_bytes(self.0[n..].to_vec()),
            None => ResponseTemplate::new(200).set_body_bytes(self.0.clone()),
        }
    }
}

#[tokio::test]
async fn a_resumed_download_requests_only_the_missing_bytes_and_starts_at_60_percent() {
    let data = weights();
    let server = MockServer::start().await;
    let rest = data[SIZE * 6 / 10..].to_vec();
    Mock::given(method("GET"))
        .and(path("/qwen-x-Q4_K_M.gguf"))
        .and(header("range", "bytes=600000-"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(rest))
        .expect(1)
        .mount(&server)
        .await;
    let home = temp();
    let jobs = Jobs::open(home.path());
    let library = Library::open(home.path());
    let mut job = job(
        &jobs,
        &library,
        &format!("{}/qwen-x-Q4_K_M.gguf", server.uri()),
        &data,
    );
    // Paused at 60%: the partial holds the first 600000 bytes.
    std::fs::write(jobs.partial(&job.id, 0), &data[..SIZE * 6 / 10]).unwrap();
    assert_eq!(jobs.view(&job).state, "paused");

    let mut seen: Vec<Progress> = Vec::new();
    let pulled = pull::run(
        &library,
        &jobs,
        &Downloader::new(client()),
        &mut job,
        &Control::default(),
        &mut |p| seen.push(p),
    )
    .await
    .unwrap();
    assert_eq!(seen[0].percent(), 60, "{seen:?}");
    assert_eq!(seen.last().unwrap().percent(), 100);
    let Pulled::Ready(model) = pulled else {
        panic!("{pulled:?}")
    };
    assert_eq!(std::fs::read(model.weights().unwrap()).unwrap(), data);
    assert!(jobs.all().is_empty(), "a finished job is removed");
    assert_eq!(library.list(&jobs)[0].state, "ready");
}

#[tokio::test]
async fn an_interrupted_download_is_partial_and_not_usable() {
    let data = weights();
    let home = temp();
    let jobs = Jobs::open(home.path());
    let library = Library::open(home.path());
    let mut job = job(&jobs, &library, "http://127.0.0.1:9/never", &data);
    std::fs::write(jobs.partial(&job.id, 0), &data[..SIZE / 3]).unwrap();
    // The process that ran it died mid-download.
    job.state = JobState::Running;
    job.owner = Some(pmt_local::download::Owner {
        pid: u32::MAX - 7,
        start_time: 1,
    });
    jobs.save(&job).unwrap();

    let listed = library.list(&jobs);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "qwen-x");
    assert_eq!(listed[0].state, "partial");
    assert_eq!(listed[0].job.as_deref(), Some(job.id.as_str()));
    let err = library.model("qwen-x", &jobs).unwrap_err();
    assert_eq!(err.exit_code(), 66);
    assert!(err.message().contains("partial"), "{}", err.message());
    assert!(
        !library
            .dir("qwen-x")
            .unwrap()
            .join("qwen-x-Q4_K_M.gguf")
            .exists(),
        "nothing published"
    );
    assert_eq!(jobs.view(&job).state, "interrupted");
}

#[tokio::test]
async fn a_pause_stops_at_a_chunk_boundary_and_resume_finishes_the_file() {
    // Large enough to arrive in many chunks.
    let data = sized(8 * SIZE);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(Ranged(data.clone()))
        .mount(&server)
        .await;
    let home = temp();
    let jobs = Jobs::open(home.path());
    let library = Library::open(home.path());
    let mut job = job(
        &jobs,
        &library,
        &format!("{}/qwen-x-Q4_K_M.gguf", server.uri()),
        &data,
    );
    let control = Control::default();
    let pause = control.clone();
    let pulled = pull::run(
        &library,
        &jobs,
        &Downloader::new(client()),
        &mut job,
        &control,
        &mut |p| {
            if p.done > 0 {
                pause.pause();
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(pulled, Pulled::Paused(job.id.clone()));
    let view = jobs.view(&jobs.load(&job.id).unwrap());
    assert_eq!(view.state, "paused");
    assert!(
        view.done_bytes > 0 && view.done_bytes < data.len() as u64,
        "{view:?}"
    );
    assert_eq!(library.list(&jobs)[0].state, "partial");

    let mut job = jobs.load(&job.id).unwrap();
    let pulled = pull::run(
        &library,
        &jobs,
        &Downloader::new(client()),
        &mut job,
        &Control::default(),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(matches!(pulled, Pulled::Ready(_)));
    let ranged = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.headers.contains_key("range"))
        .count();
    assert_eq!(ranged, 1, "the resume asked for the missing range");
    assert_eq!(
        std::fs::read(library.dir("qwen-x").unwrap().join("qwen-x-Q4_K_M.gguf")).unwrap(),
        data
    );
}

#[tokio::test]
async fn a_file_that_differs_from_its_pin_is_never_published() {
    let data = weights();
    let mut bad = data.clone();
    bad[10] ^= 1;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bad))
        .mount(&server)
        .await;
    let home = temp();
    let jobs = Jobs::open(home.path());
    let library = Library::open(home.path());
    let mut job = job(
        &jobs,
        &library,
        &format!("{}/qwen-x-Q4_K_M.gguf", server.uri()),
        &data,
    );
    let err = pull::run(
        &library,
        &jobs,
        &Downloader::new(client()),
        &mut job,
        &Control::default(),
        &mut |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 74);
    assert!(
        err.message().contains("qwen-x-Q4_K_M.gguf"),
        "{}",
        err.message()
    );
    assert!(
        !library
            .dir("qwen-x")
            .unwrap()
            .join("qwen-x-Q4_K_M.gguf")
            .exists()
    );
    assert!(
        !jobs.partial(&job.id, 0).exists(),
        "the bad bytes were deleted"
    );
    assert_eq!(jobs.view(&jobs.load(&job.id).unwrap()).state, "failed");
}
