//! Evolution over the client protocol and the daemon's runner (spec:
//! harness-evolution, Job lifecycle, events and RPC; Resume and
//! content-addressed session cache; experience-library, Library CLI).

#![cfg(all(feature = "evolve", feature = "gateway"))]

mod common;

use std::time::Duration;

use common::{append_config, daemon, mock_home, ok, run};
use serde_json::{Value, json};

const MOCK: &str = include_str!("fixtures/evolve/mock.toml");

fn setup() -> tempfile::TempDir {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "evolve.enabled", "true"],
    ));
    std::fs::write(home.path().join("profiles/default/mock.toml"), MOCK).unwrap();
    append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    std::fs::write(
        home.path().join("task.toml"),
        "statement = \"TASK-SUM the ledger\"\ndomain = \"accounting\"\n",
    )
    .unwrap();
    home
}

async fn call(client: &pmt_rpc::client::Client, method: &str, params: Value) -> Value {
    client
        .call(method, params)
        .await
        .unwrap_or_else(|e| panic!("{method}: {e:?}"))
}

/// Waits until job `id` reaches a final status.
async fn ended(client: &pmt_rpc::client::Client, id: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let status = call(client, "evolve.status", json!({"job_id": id})).await;
        let state = status["data"]["job"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !matches!(state.as_str(), "queued" | "running") {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job {id} still {state}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[test]
fn a_job_started_over_rpc_runs_in_the_daemon_and_its_proposal_is_adopted() {
    let home = setup();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let task = home.path().join("task.toml");
        let started = call(&client, "evolve.start", json!({"task_file": task, "k": 1})).await;
        assert!(
            started["message"].as_str().unwrap().contains("queued (k=1"),
            "{started}"
        );
        let id = started["data"]["id"].as_str().unwrap().to_owned();
        let status = ended(&client, &id).await;
        assert_eq!(status["data"]["summary"]["status"], "ok", "{status}");
        let list = call(&client, "evolve.list", json!({})).await;
        let row = &list["jobs"][0];
        assert_eq!(
            (
                row["id"].as_str(),
                row["status"].as_str(),
                row["rollouts"].as_u64()
            ),
            (Some(id.as_str()), Some("ok"), Some(4))
        );
        let proposals = call(&client, "evolve.proposals.list", json!({})).await;
        let proposal = proposals["proposals"][0]["id"].as_str().unwrap().to_owned();
        let adopted = call(
            &client,
            "evolve.proposals.adopt",
            json!({"proposal_id": proposal}),
        )
        .await;
        let to = adopted["data"]["to"].as_str().unwrap().to_owned();
        let log = call(&client, "harness.log", json!({})).await;
        assert_eq!(log["active"], to.as_str());
        let diff = call(&client, "harness.diff", json!({"version": "h1"})).await;
        assert!(
            diff["data"]["diff"]
                .as_str()
                .unwrap()
                .contains("+Always state the total"),
            "{diff}"
        );
        library_over_rpc(&client, &adopted).await;
        let err = client
            .call("evolve.cancel", json!({"job_id": id}))
            .await
            .unwrap_err();
        assert_eq!(err.data.unwrap().code, "evolve.job_state");
    });
}

/// The adoption filed the job's lesson; the experience library answers
/// over RPC, and the rollback demotes the entry (spec:
/// experience-library, Library CLI: the RPC equivalents).
async fn library_over_rpc(client: &pmt_rpc::client::Client, adopted: &Value) {
    assert!(
        adopted["message"]
            .as_str()
            .unwrap()
            .contains("added to domain accounting"),
        "{adopted}"
    );
    let domains = call(client, "experience.domains", json!({})).await;
    assert_eq!(
        domains,
        json!({"domains": [{"name": "accounting", "entries": 1}]})
    );
    let listed = call(client, "experience.list", json!({"domain": "accounting"})).await;
    let entry = listed["entries"][0]["entry_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = call(client, "experience.show", json!({"entry_id": entry})).await;
    assert!(
        shown["data"]["document"]
            .as_str()
            .unwrap()
            .contains("## Transferable lesson"),
        "{shown}"
    );
    let pinned = call(client, "experience.pin", json!({"entry_id": entry})).await;
    assert_eq!(pinned["data"]["pinned"], true);
    call(
        client,
        "experience.pin",
        json!({"entry_id": entry, "pinned": false}),
    )
    .await;
    let rolled = call(client, "harness.rollback", json!({"version": "h1"})).await;
    assert!(
        rolled["message"]
            .as_str()
            .unwrap()
            .contains("rollback of h1"),
        "{rolled}"
    );
    assert_eq!(rolled["data"]["demoted_entries"], json!([entry]));
    let hidden = call(client, "experience.list", json!({})).await;
    assert_eq!(hidden["entries"], json!([]));
    let restored = call(client, "experience.restore", json!({"entry_id": entry})).await;
    assert_eq!(restored["data"]["status"], "active");
    call(client, "experience.forget", json!({"entry_id": entry})).await;
    let err = client
        .call("experience.show", json!({"entry_id": entry}))
        .await
        .unwrap_err();
    assert_eq!(err.data.unwrap().code, "experience.entry_not_found");
}

/// Scenario: Resume after crash (the daemon queues an interrupted job
/// again at start and finishes it).
#[test]
fn the_daemon_resumes_an_interrupted_job_at_start() {
    let home = setup();
    let out = run(
        home.path(),
        &[
            "evolve",
            "run",
            "--task",
            home.path().join("task.toml").to_str().unwrap(),
            "--k",
            "1",
            "--detach",
        ],
    );
    let text = ok(&out);
    let id = text.split_whitespace().nth(1).unwrap().to_owned();
    // A process that died while running it left the job `running`.
    let job = home
        .path()
        .join("profiles/default/evolve/jobs")
        .join(&id)
        .join("job.toml");
    let record = std::fs::read_to_string(&job)
        .unwrap()
        .replace("status = \"queued\"", "status = \"running\"");
    std::fs::write(&job, record).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let status = ended(&client, &id).await;
        assert_eq!(status["data"]["summary"]["status"], "ok", "{status}");
    });
}

/// Ids are checked before they reach a path: a connection granted one
/// profile cannot read another's job or harness version through `../`.
#[test]
fn ids_that_leave_the_profile_are_not_found() {
    let home = setup();
    ok(&run(home.path(), &["profile", "create", "other"]));
    let out = run(
        home.path(),
        &[
            "evolve",
            "run",
            "--task",
            home.path().join("task.toml").to_str().unwrap(),
            "--detach",
        ],
    );
    let id = ok(&out).split_whitespace().nth(1).unwrap().to_owned();
    // The job now belongs to the other profile.
    let profiles = home.path().join("profiles");
    let theirs = profiles.join("other/evolve/jobs");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::rename(
        profiles.join("default/evolve/jobs").join(&id),
        theirs.join(&id),
    )
    .unwrap();
    let version = std::fs::read_to_string(profiles.join("other/harness/ACTIVE")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let job = format!("../../../other/evolve/jobs/{id}");
        let err = client
            .call("evolve.status", json!({"job_id": job}))
            .await
            .unwrap_err();
        assert_eq!(err.data.unwrap().code, "evolve.job_not_found");
        let version = format!("../../../other/harness/versions/{}", version.trim());
        let err = client
            .call("harness.diff", json!({"version": version}))
            .await
            .unwrap_err();
        assert_eq!(err.data.unwrap().code, "harness.version_not_found");
    });
}
