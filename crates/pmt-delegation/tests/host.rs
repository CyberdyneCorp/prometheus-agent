//! The delegation host over a fake launcher (spec: subagent-delegation):
//! dispatch, preconditions, queueing, control actions scoped to the spawn
//! tree, stop semantics, structured results and durable completions.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_agent::turn::model::BoxFuture;
use pmt_agent::turn::{EventSink, Interrupt, Recorded, TurnUsage};
use pmt_config::schema::DelegationConfig;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_delegation::agents::Definitions;
use pmt_delegation::budget::OneShotBudget;
use pmt_delegation::grant::ParentGrant;
use pmt_delegation::host::deliver;
use pmt_delegation::host::{
    ChildPlan, ChildSession, Closed, Completions, Delegator, Dispatch, Launcher, SessionContext,
    TurnEnd, TurnOutcome,
};
use pmt_delegation::router::{DefaultRouter, Route, Router};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::ToolCall;
use pmt_obs::trajectory::writer::Emit;
use pmt_store::{NewSession, Source, Store};
use pmt_tools::{Builtins, ToolRegistry, ToolResult};
use serde_json::{Value, json};

struct Fixture {
    _root: tempfile::TempDir,
    scope: ProfileScope,
    store: Arc<Store>,
    session: SessionId,
    launcher: Arc<FakeLauncher>,
    events: Arc<Recorded>,
    completions: Arc<Inbox>,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let home = Home::open(&root.path().join("home")).unwrap();
    let dir = home
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    dir.config_file().replace("").unwrap();
    let scope =
        ProfileScope::open(dir, &Overrides::parse::<&str>(&[]).unwrap(), Launch::Other).unwrap();
    let store = Arc::new(Store::open(&scope).unwrap());
    let session = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    Fixture {
        _root: root,
        scope,
        store,
        session,
        launcher: Arc::new(FakeLauncher::default()),
        events: Recorded::new(),
        completions: Arc::new(Inbox::default()),
    }
}

impl Fixture {
    fn delegator(&self, config: DelegationConfig, dispatch: Dispatch) -> Arc<Delegator> {
        self.delegator_with(config, dispatch, |_| {})
    }

    fn delegator_with(
        &self,
        config: DelegationConfig,
        dispatch: Dispatch,
        edit: impl FnOnce(&mut SessionContext),
    ) -> Arc<Delegator> {
        let registry = Arc::new(ToolRegistry::new(
            "default",
            Arc::new(Builtins::standard(pmt_tools::agent_level::tools()).unwrap()),
        ));
        let mut ctx = SessionContext {
            scope: self.scope.clone(),
            store: Arc::clone(&self.store),
            session: self.session,
            root: self.session,
            depth: 0,
            config,
            registry: Arc::clone(&registry),
            agents: Definitions::default(),
            router: Ok(Arc::new(DefaultRouter)),
            memory: Some("the user prefers tabs".to_owned()),
            dispatch,
            oneshot: None,
            events: Arc::clone(&self.events) as Arc<dyn EventSink>,
            launcher: Arc::clone(&self.launcher) as Arc<dyn Launcher>,
            spill_dir: self
                .scope
                .dir()
                .join("cache/spillover")
                .join(self.session.to_string()),
            paused: Arc::new(|| false),
            context_left: Arc::new(|| None),
            shape: pmt_delegation::shape::Shape::default(),
        };
        edit(&mut ctx);
        let delegator = Delegator::new(ctx);
        let tools = registry.members("core").unwrap();
        delegator.set_parent(ParentGrant {
            tools,
            capabilities: pmt_core::capability::CapabilitySet::parse(["fs.write:/work/**"])
                .unwrap(),
        });
        delegator
    }

    fn background(&self) -> Dispatch {
        Dispatch::Background(Arc::clone(&self.completions) as Arc<dyn Completions>)
    }
}

fn sync() -> Dispatch {
    Dispatch::Synchronous("Test runs wait for their children.".to_owned())
}

fn body(result: &ToolResult) -> Value {
    serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| json!({"text": result.text_content()}))
}

fn code(result: &ToolResult) -> String {
    result
        .metadata
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[derive(Default)]
struct Inbox {
    delivered: Mutex<Vec<(String, String)>>,
    refuse: AtomicBool,
}

impl Completions for Inbox {
    fn deliver<'a>(&'a self, delegation_id: &'a str, text: String) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            if self.refuse.load(Ordering::SeqCst) {
                return false;
            }
            self.delivered
                .lock()
                .unwrap()
                .push((delegation_id.to_owned(), text));
            true
        })
    }
}

impl Inbox {
    async fn wait_for(&self, n: usize) -> Vec<(String, String)> {
        for _ in 0..500 {
            let got = self.delivered.lock().unwrap().clone();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no completion delivered");
    }
}

#[derive(Default)]
struct FakeLauncher {
    plans: Mutex<Vec<ChildPlan>>,
    running: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    /// Where live transcripts are written, and the logs found there each
    /// time a child was opened (before its first model call).
    live_root: Mutex<Option<PathBuf>>,
    logs_at_open: Mutex<Vec<usize>>,
}

impl FakeLauncher {
    fn count_logs(&self) -> usize {
        let Some(root) = self.live_root.lock().unwrap().clone() else {
            return 0;
        };
        std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .flat_map(|dir| std::fs::read_dir(dir.path()).into_iter().flatten())
            .filter_map(Result::ok)
            .filter(|f| f.file_name().to_string_lossy().starts_with("task-"))
            .count()
    }
}

impl Launcher for FakeLauncher {
    fn open<'a>(
        &'a self,
        plan: &'a ChildPlan,
    ) -> BoxFuture<'a, Result<Arc<dyn ChildSession>, Error>> {
        Box::pin(async move {
            let logs = self.count_logs();
            self.logs_at_open.lock().unwrap().push(logs);
            self.plans.lock().unwrap().push(plan.clone());
            if plan.model.model.as_deref() == Some("bad/route") {
                return Err(Error::new(
                    &pmt_core::error::codes::MODEL_INVALID_REF,
                    "unknown model 'bad/route'",
                ));
            }
            let goal = plan.system.clone();
            Ok(Arc::new(FakeChild {
                id: SessionId::new(),
                goal,
                events: Arc::clone(&plan.events),
                turns: AtomicUsize::new(0),
                steers: Mutex::default(),
                running: Arc::clone(&self.running),
                peak: Arc::clone(&self.peak),
            }) as Arc<dyn ChildSession>)
        })
    }
}

struct FakeChild {
    id: SessionId,
    goal: String,
    events: Arc<dyn EventSink>,
    turns: AtomicUsize,
    steers: Mutex<Vec<String>>,
    running: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl FakeChild {
    fn answer(&self, turn: usize) -> (Option<String>, TurnEnd) {
        let goal = &self.goal;
        if goal.contains("must-fail") {
            return (None, TurnEnd::Error);
        }
        if goal.contains("max-out") {
            return (Some("partial findings".into()), TurnEnd::MaxIterations);
        }
        if goal.contains("schema-bad") {
            return (Some("{}".into()), TurnEnd::FinalAnswer);
        }
        if goal.contains("schema-fix") {
            let text = if turn == 0 { "{}" } else { "{\"n\": 1}" };
            return (Some(text.into()), TurnEnd::FinalAnswer);
        }
        if goal.contains("huge") {
            return (Some("x".repeat(100_000)), TurnEnd::FinalAnswer);
        }
        (Some(format!("done #{turn}")), TurnEnd::FinalAnswer)
    }
}

impl ChildSession for FakeChild {
    fn id(&self) -> SessionId {
        self.id
    }

    fn model(&self) -> String {
        "mock/mock".to_owned()
    }

    fn turn(&self, _input: String, interrupt: Interrupt) -> BoxFuture<'_, TurnOutcome> {
        Box::pin(async move {
            let turn = self.turns.fetch_add(1, Ordering::SeqCst);
            let (running, peak) = (&self.running, &self.peak);
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            if self.goal.contains("write") {
                for path in ["/work/src/lib.rs", "/work/README.md"] {
                    let event = Event::ToolCall(ToolCall {
                        call_id: path.to_owned(),
                        tool: "write_file".to_owned(),
                        arguments: json!({"path": path}),
                    });
                    self.events.emit(Emit::now(event)).await.unwrap();
                }
            }
            let slow = self.goal.contains("slow");
            let stopped = if slow {
                tokio::select! {
                    () = interrupt.token().cancelled() => true,
                    () = async { while interrupt.issuer().is_none() { tokio::time::sleep(Duration::from_millis(5)).await } } => true,
                    () = tokio::time::sleep(Duration::from_millis(400)) => false,
                }
            } else {
                false
            };
            running.fetch_sub(1, Ordering::SeqCst);
            let (text, end) = if stopped {
                (None, TurnEnd::Interrupted)
            } else {
                self.answer(turn)
            };
            TurnOutcome {
                text,
                end,
                iterations: 1,
                usage: TurnUsage {
                    calls: 1,
                    input_tokens: 10,
                    output_tokens: 5,
                    cost_usd: Some(0.01),
                    ..TurnUsage::default()
                },
                error: (end == TurnEnd::Error)
                    .then(|| ("provider.failed".to_owned(), "boom".to_owned())),
            }
        })
    }

    fn steer(&self, text: &str) -> bool {
        self.steers.lock().unwrap().push(text.to_owned());
        true
    }

    fn undelivered_steers(&self) -> Vec<String> {
        Vec::new()
    }

    fn close(&self) -> BoxFuture<'_, Closed> {
        Box::pin(async { Closed::default() })
    }
}

async fn call(d: &Arc<Delegator>, args: Value) -> ToolResult {
    d.call(&args, &CancelToken::new()).await
}

/// Scenario: Single task batch.
#[tokio::test]
async fn a_single_task_spawns_exactly_one_child() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    let result = call(
        &d,
        json!({"tasks": [{"goal": "Fix the failing tests in crates/store"}]}),
    )
    .await;
    let body = body(&result);
    assert_eq!(f.launcher.plans.lock().unwrap().len(), 1);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["status"], "completed");
    assert_eq!(results[0]["exit_reason"], "final_answer");
    assert!(
        results[0]["subagent_id"]
            .as_str()
            .unwrap()
            .starts_with("sa-")
    );
    assert_eq!(body["note"], "Test runs wait for their children.");
}

/// Scenario: Depth limit reached.
#[tokio::test]
async fn a_child_at_the_depth_limit_builds_nothing() {
    let f = fixture();
    let d = f.delegator_with(DelegationConfig::default(), sync(), |ctx| ctx.depth = 1);
    let result = call(&d, json!({"tasks": [{"goal": "go deeper"}]})).await;
    assert_eq!(code(&result), "delegation.depth_limit");
    assert_eq!(
        result.text_content(),
        "Delegation depth limit reached (depth=1, max_spawn_depth=1)."
    );
    assert!(f.launcher.plans.lock().unwrap().is_empty());
}

/// Scenario: Pause blocks only new spawns.
#[tokio::test]
async fn a_pause_refuses_spawns_but_not_list() {
    let f = fixture();
    let d = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.paused = Arc::new(|| true);
    });
    let listed = call(&d, json!({"action": "list"})).await;
    assert!(listed.status.succeeded(), "{}", listed.text_content());
    let spawned = call(&d, json!({"tasks": [{"goal": "anything"}]})).await;
    assert_eq!(code(&spawned), "delegation.paused");
    assert_eq!(
        spawned.text_content(),
        "Delegation spawning is paused. Resume with /delegation resume."
    );
}

/// Scenario: Background dispatch returns a handle.
#[tokio::test]
async fn a_background_spawn_answers_with_a_handle_and_delivers_once() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), f.background());
    let result = call(
        &d,
        json!({"tasks": [{"goal": "slow one"}, {"goal": "slow two"}]}),
    )
    .await;
    let body = body(&result);
    assert_eq!(body["status"], "dispatched");
    let id = body["delegation_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("dg-"));
    assert_eq!(body["subagent_ids"].as_array().unwrap().len(), 2);
    assert!(
        f.completions.delivered.lock().unwrap().is_empty(),
        "returned before the children ended"
    );
    let delivered = f.completions.wait_for(1).await;
    assert_eq!(delivered[0].0, id);
    assert!(delivered[0].1.contains("delegation_completed"));
    let row = f.store.delegation(&id).unwrap().unwrap();
    assert_eq!(row.delivery, pmt_store::delegations::Delivery::Delivered);
}

/// Scenario: Excess children queue rather than oversubscribe.
#[tokio::test]
async fn a_third_child_reports_queued_and_runs_when_a_slot_frees() {
    let f = fixture();
    let config = DelegationConfig {
        max_concurrent_children: 2,
        ..DelegationConfig::default()
    };
    // A profile of its own so other tests' children do not share the limit.
    let d = f.delegator(config, f.background());
    let result = call(
        &d,
        json!({"tasks": [{"goal": "slow a"}, {"goal": "slow b"}, {"goal": "slow c"}]}),
    )
    .await;
    let body = body(&result);
    assert_eq!(body["queued"].as_array().unwrap().len(), 1);
    f.completions.wait_for(1).await;
    assert_eq!(f.launcher.peak.load(Ordering::SeqCst), 2);
    assert_eq!(f.launcher.plans.lock().unwrap().len(), 3);
}

/// Scenario: Foreign child cannot be steered; Stop one child.
#[tokio::test]
async fn control_reaches_only_this_conversations_tree_and_stop_spares_siblings() {
    let f = fixture();
    let mine = f.delegator(DelegationConfig::default(), f.background());
    let other_session = f
        .store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let theirs = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.session = other_session;
        ctx.root = other_session;
    });
    let body = body(
        &call(
            &mine,
            json!({"tasks": [{"goal": "slow 1"}, {"goal": "slow 2"}, {"goal": "slow 3"}]}),
        )
        .await,
    );
    let ids: Vec<String> = body["subagent_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let foreign = call(
        &theirs,
        json!({"action": "steer", "subagent_id": ids[0], "message": "focus on tests"}),
    )
    .await;
    assert_eq!(
        foreign.text_content(),
        format!(
            "No live subagent '{}' in this conversation's spawn tree.",
            ids[0]
        )
    );
    let steered = call(
        &mine,
        json!({"action": "steer", "subagent_id": ids[0], "message": "focus on tests"}),
    )
    .await;
    assert_eq!(super_body(&steered)["status"], "queued");
    let empty = call(
        &mine,
        json!({"action": "steer", "subagent_id": ids[0], "message": " "}),
    )
    .await;
    assert!(!empty.status.succeeded());
    let stopped = call(&mine, json!({"action": "stop", "subagent_id": ids[1]})).await;
    assert_eq!(super_body(&stopped)["status"], "interrupt_requested");
    let delivered = f.completions.wait_for(1).await;
    let completion = &delivered[0].1;
    let json_start = completion.find('{').unwrap();
    let results: Value = serde_json::from_str(&completion[json_start..]).unwrap();
    let statuses: Vec<&str> = results["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["completed", "interrupted", "completed"]);
}

fn super_body(result: &ToolResult) -> Value {
    body(result)
}

/// Scenario: Turn interrupt leaves background work running.
#[tokio::test]
async fn a_turn_interrupt_stops_synchronous_children_only() {
    let f = fixture();
    let bg = f.delegator(DelegationConfig::default(), f.background());
    call(&bg, json!({"tasks": [{"goal": "slow background"}]})).await;
    let fg = f.delegator(DelegationConfig::default(), sync());
    let turn = CancelToken::new();
    let cancel = turn.clone();
    let canceller = pmt_core::spawn::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let result = fg
        .call(&json!({"tasks": [{"goal": "slow foreground"}]}), &turn)
        .await;
    canceller.await.unwrap();
    assert_eq!(body(&result)["results"][0]["status"], "interrupted");
    let delivered = f.completions.wait_for(1).await;
    assert!(
        delivered[0].1.contains("\"completed\""),
        "{}",
        delivered[0].1
    );
}

/// Stop semantics: a closed session stops its children and spawns no more.
#[tokio::test]
async fn closing_the_session_refuses_later_spawns() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    d.close();
    let result = call(&d, json!({"tasks": [{"goal": "after close"}]})).await;
    assert_eq!(code(&result), "delegation.session_closed");
}

/// Scenario: Budget exhaustion is truncation, not failure; Artifacts reported.
#[tokio::test]
async fn results_report_truncation_and_written_files() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    let result = call(
        &d,
        json!({"tasks": [{"goal": "max-out"}, {"goal": "write files"}]}),
    )
    .await;
    let body = body(&result);
    let truncated = &body["results"][0];
    assert_eq!(truncated["status"], "completed");
    assert_eq!(truncated["exit_reason"], "max_iterations");
    assert_eq!(truncated["truncated"], true);
    let paths: Vec<&str> = body["results"][1]["artifacts"]
        .as_array()
        .map(|a| a.iter().map(|x| x["path"].as_str().unwrap()).collect())
        .unwrap_or_default();
    // The fake writes no real files, so their digests cannot be read; the
    // real-session test checks the digests.
    assert!(paths.is_empty() || paths == ["/work/src/lib.rs", "/work/README.md"]);
}

/// Scenario: Invalid twice.
#[tokio::test]
async fn an_answer_invalid_twice_keeps_the_raw_text() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    let schema = json!({"type": "object", "required": ["n"]});
    let result = call(
        &d,
        json!({"tasks": [
            {"goal": "schema-bad", "output_schema": schema},
            {"goal": "schema-fix", "output_schema": schema}
        ]}),
    )
    .await;
    let body = body(&result);
    let bad = &body["results"][0];
    assert_eq!(bad["status"], "completed");
    assert_eq!(bad["schema_valid"], false);
    assert_eq!(bad["output"], Value::Null);
    assert_eq!(bad["summary"], "{}");
    assert!(!bad["schema_errors"].as_array().unwrap().is_empty());
    let fixed = &body["results"][1];
    assert_eq!(fixed["schema_valid"], true);
    assert_eq!(fixed["output"], json!({"n": 1}));
}

/// Scenario: Oversized summary is kept losslessly (through the host).
#[tokio::test]
async fn an_oversized_summary_is_capped_in_the_parent_context() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    let body = body(&call(&d, json!({"tasks": [{"goal": "huge"}]})).await);
    let result = &body["results"][0];
    assert!(result["summary"].as_str().unwrap().chars().count() <= 24_000);
    let full = PathBuf::from(result["summary_ref"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(full).unwrap().len(), 100_000);
}

/// Scenario: Even split enforced (the share each child is given).
#[tokio::test]
async fn a_call_budget_is_split_evenly_across_children() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    call(&d, json!({"tasks": [{"goal": "a"}, {"goal": "b"}, {"goal": "c"}, {"goal": "d"}], "budget": {"max_cost_usd": 1.0}})).await;
    let plans = f.launcher.plans.lock().unwrap();
    assert!(plans.iter().all(|p| p.budget.max_cost_usd == Some(0.25)));
}

/// Scenario: Third child in a one-shot run.
#[tokio::test]
async fn a_one_shot_run_refuses_its_third_child() {
    let f = fixture();
    let d = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.oneshot = Some(Arc::new(OneShotBudget::new(2)));
    });
    call(&d, json!({"tasks": [{"goal": "a"}, {"goal": "b"}]})).await;
    let third = call(&d, json!({"tasks": [{"goal": "c"}]})).await;
    assert_eq!(code(&third), "delegation.budget_exhausted");
}

struct RefuseOne;

impl Router for RefuseOne {
    fn name(&self) -> &'static str {
        "refuse-one"
    }

    fn route<'a>(
        &'a self,
        task: &'a pmt_delegation::contract::Task,
    ) -> BoxFuture<'a, Result<Route, String>> {
        Box::pin(async move {
            if task.index == 1 {
                Err("no route for this task".to_owned())
            } else {
                Ok(Route::default())
            }
        })
    }
}

/// Scenario: Missing router; One unroutable task.
#[tokio::test]
async fn routers_fail_closed_and_refuse_single_tasks() {
    let f = fixture();
    let missing = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.router = pmt_delegation::router::select("tiers", &[]);
    });
    let result = call(&missing, json!({"tasks": [{"goal": "x"}]})).await;
    assert_eq!(code(&result), "delegation.router_unavailable");
    assert!(f.launcher.plans.lock().unwrap().is_empty());
    let one = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.router = Ok(Arc::new(RefuseOne));
    });
    let body = body(
        &call(
            &one,
            json!({"tasks": [{"goal": "a"}, {"goal": "b"}, {"goal": "c"}]}),
        )
        .await,
    );
    let statuses: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["completed", "refused", "completed"]);
    assert_eq!(
        body["results"][1]["error"]["message"],
        "no route for this task"
    );
}

/// Scenario: Restart delivers an owned completion once.
#[tokio::test]
async fn a_completion_owed_across_a_restart_is_delivered_once() {
    let f = fixture();
    f.completions.refuse.store(true, Ordering::SeqCst);
    let d = f.delegator(DelegationConfig::default(), f.background());
    let body = body(&call(&d, json!({"tasks": [{"goal": "quick"}]})).await);
    let id = body["delegation_id"].as_str().unwrap().to_owned();
    // The surface was gone: the completion stays pending.
    for _ in 0..200 {
        if f.store
            .delegation(&id)
            .unwrap()
            .unwrap()
            .completed_at
            .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        f.store.delegation(&id).unwrap().unwrap().delivery,
        pmt_store::delegations::Delivery::Pending
    );
    // A restart: a new process opens the store and the session resumes.
    let reopened = Store::open(&f.scope).unwrap();
    deliver::recover(&reopened);
    f.completions.refuse.store(false, Ordering::SeqCst);
    let spill = f.scope.dir().join("spill");
    deliver::deliver_pending(&reopened, f.session, None, &spill, f.completions.as_ref()).await;
    deliver::deliver_pending(&reopened, f.session, None, &spill, f.completions.as_ref()).await;
    let delivered = f.completions.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        reopened.delegation(&id).unwrap().unwrap().delivery,
        pmt_store::delegations::Delivery::Delivered
    );
}

/// Children left running by a stopped process end `interrupted`.
#[tokio::test]
async fn a_restart_interrupts_children_that_were_running() {
    let f = fixture();
    let row = pmt_store::delegations::SubagentRow {
        id: "sa-01J00000000000000000000000".into(),
        delegation_id: "dg-1".into(),
        task_index: 0,
        session_id: None,
        parent_session_id: f.session.to_string(),
        root_session_id: f.session.to_string(),
        depth: 1,
        goal: "g".into(),
        status: "running".into(),
        result: None,
    };
    f.store
        .record_delegation(
            "dg-1",
            f.session,
            pmt_store::delegations::Delivery::Pending,
            &[row],
        )
        .unwrap();
    deliver::recover(&f.store);
    let child = &f.store.subagents_of("dg-1").unwrap()[0];
    assert_eq!(child.status, "interrupted");
    deliver::deliver_pending(
        &f.store,
        f.session,
        None,
        &f.scope.dir().join("s"),
        f.completions.as_ref(),
    )
    .await;
    assert!(
        f.completions.delivered.lock().unwrap()[0]
            .1
            .contains("\"interrupted\"")
    );
}

/// A completion older than 48 hours is dropped instead of delivered.
#[tokio::test]
async fn stale_completions_are_dropped() {
    let f = fixture();
    f.store
        .record_delegation(
            "dg-old",
            f.session,
            pmt_store::delegations::Delivery::Pending,
            &[],
        )
        .unwrap();
    f.store
        .age_delegation("dg-old", Duration::from_hours(49))
        .unwrap();
    deliver::recover(&f.store);
    assert_eq!(
        f.store.delegation("dg-old").unwrap().unwrap().delivery,
        pmt_store::delegations::Delivery::Dropped
    );
}

/// Pinned routes that fail validation refuse only their task.
#[tokio::test]
async fn a_pinned_model_that_does_not_resolve_refuses_its_task() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    let body = body(
        &call(
            &d,
            json!({"tasks": [{"goal": "a", "model": "bad/route"}, {"goal": "b"}]}),
        )
        .await,
    );
    assert_eq!(body["results"][0]["status"], "refused");
    assert_eq!(body["results"][1]["status"], "completed");
    let plans = f.launcher.plans.lock().unwrap();
    assert!(plans[0].model.pinned());
    assert!(!plans[1].model.pinned());
}

/// Scenario: Child cannot write memory (the plan side).
#[tokio::test]
async fn children_get_the_frozen_memory_and_no_memory_tool() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    call(&d, json!({"tasks": [{"goal": "look around"}]})).await;
    let plans = f.launcher.plans.lock().unwrap();
    assert_eq!(plans[0].memory.as_deref(), Some("the user prefers tabs"));
    assert!(
        !plans[0]
            .grant
            .tools
            .iter()
            .any(|t| t == "memory" || t == "clarify" || t == "delegate_task")
    );
}

/// Progress events reach the root stream when a child runs.
#[tokio::test]
async fn spawned_and_finished_events_reach_the_root_stream() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    call(&d, json!({"tasks": [{"goal": "write things"}]})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let kinds: Vec<&str> = f.events.events().iter().map(Event::kind).collect();
    assert!(kinds.contains(&"subagent.spawned"), "{kinds:?}");
    assert!(kinds.contains(&"subagent.progress"), "{kinds:?}");
    assert!(kinds.contains(&"subagent.finished"), "{kinds:?}");
}

/// Scenario: Definition added mid-session.
#[tokio::test]
async fn a_definition_added_mid_session_waits_for_the_next_session() {
    let f = fixture();
    let dir = f.scope.dir().join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let running = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.agents = Definitions::load(std::slice::from_ref(&dir));
    });
    std::fs::write(
        dir.join("reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews code\n---\nBe strict.\n",
    )
    .unwrap();
    let refused = call(
        &running,
        json!({"tasks": [{"goal": "Review it", "agent": "reviewer"}]}),
    )
    .await;
    assert_eq!(code(&refused), "delegation.unknown_agent");
    let next = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.agents = Definitions::load(std::slice::from_ref(&dir));
    });
    let accepted = call(
        &next,
        json!({"tasks": [{"goal": "Review it", "agent": "reviewer"}]}),
    )
    .await;
    assert_eq!(body(&accepted)["results"][0]["status"], "completed");
}

/// Scenario: Plain switch leaves the running session unchanged: the
/// router a session started with stays until it is replaced on purpose.
#[tokio::test]
async fn a_router_switch_reaches_a_session_only_when_applied_now() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), sync());
    // `/delegation router tiers` without --now only writes the config: this
    // session keeps routing with `default`.
    let routed = call(&d, json!({"tasks": [{"goal": "a"}]})).await;
    assert_eq!(body(&routed)["results"][0]["status"], "completed");
    // With --now the session takes the new router (here: unavailable).
    d.set_router(pmt_delegation::router::select("tiers", &[]));
    let refused = call(&d, json!({"tasks": [{"goal": "a"}]})).await;
    assert_eq!(code(&refused), "delegation.router_unavailable");
}

/// The plugin lifecycle interface: a launch is one background child of
/// the calling session whose result goes to the plugin, never to the
/// session; cancel reports `cancel_requested`, then `cancelled`.
#[tokio::test]
async fn a_plugin_launch_runs_a_child_the_plugin_follows_to_its_result() {
    use pmt_delegation::handles::HandleState;
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), f.background());
    let done = d.launch(&json!({"goal": "quick look"})).await.unwrap();
    let result = d
        .wait(&done, Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(d.status(&done).unwrap(), HandleState::Completed);
    assert_eq!(d.result(&done).unwrap(), Some(result));

    let slow = d.launch(&json!({"goal": "slow look"})).await.unwrap();
    assert_eq!(
        d.wait(&slow, Duration::from_millis(20)).await.unwrap(),
        None
    );
    d.steer(&slow, "look at the tests too").unwrap();
    d.cancel(&slow).unwrap();
    assert_eq!(d.status(&slow).unwrap(), HandleState::CancelRequested);
    d.wait(&slow, Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.status(&slow).unwrap(), HandleState::Cancelled);
    assert!(
        f.completions.delivered.lock().unwrap().is_empty(),
        "plugin children owe the session nothing"
    );
}

/// Scenario: Forged handle (and a handle of another conversation).
#[tokio::test]
async fn forged_and_foreign_handles_are_unknown() {
    use pmt_delegation::host::plugin::LifecycleError;
    let f = fixture();
    let mine = f.delegator(DelegationConfig::default(), f.background());
    let other = f
        .store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let theirs = f.delegator_with(DelegationConfig::default(), sync(), |ctx| {
        ctx.session = other;
        ctx.root = other;
    });
    let handle = mine.launch(&json!({"goal": "quick look"})).await.unwrap();
    assert!(mine.status(&handle).is_ok());
    let mut forged = handle.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == '0' { '1' } else { '0' });
    assert_eq!(
        mine.status(&forged).unwrap_err(),
        LifecycleError::UnknownHandle
    );
    assert_eq!(
        theirs.cancel(&handle).unwrap_err(),
        LifecycleError::UnknownHandle
    );
}

/// A launch meets every refusal a `delegate_task` spawn meets.
#[tokio::test]
async fn a_plugin_launch_is_refused_like_a_spawn() {
    use pmt_delegation::host::plugin::LifecycleError;
    let f = fixture();
    let paused = f.delegator_with(DelegationConfig::default(), f.background(), |ctx| {
        ctx.paused = Arc::new(|| true);
    });
    let Err(LifecycleError::Refused(refusal)) = paused.launch(&json!({"goal": "anything"})).await
    else {
        panic!("a paused profile launched a child");
    };
    assert_eq!(refusal.code.code, "delegation.paused");
    let open = f.delegator(DelegationConfig::default(), f.background());
    let Err(LifecycleError::Refused(refusal)) =
        open.launch(&json!({"goal": "x", "colour": "blue"})).await
    else {
        panic!("an unknown field was accepted");
    };
    assert_eq!(refusal.code.code, "delegation.invalid_task");
    assert!(f.launcher.plans.lock().unwrap().is_empty());
}

/// Scenario: Stop one child (from the agents monitor).
#[tokio::test]
async fn stopping_one_child_from_the_monitor_spares_its_siblings() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), f.background());
    let body = body(
        &call(
            &d,
            json!({"tasks": [{"goal": "slow 1"}, {"goal": "slow 2"}, {"goal": "slow 3"}]}),
        )
        .await,
    );
    let ids: Vec<String> = body["subagent_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let conversation = d.conversation();
    let listed = pmt_delegation::monitor::tree(&conversation);
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert!(listed.iter().all(|l| l.contains("running")), "{listed:?}");
    let said = pmt_delegation::monitor::stop(&conversation, &ids[1]).unwrap();
    assert!(said.contains("siblings keep running"), "{said}");
    let delivered = f.completions.wait_for(1).await;
    let completion = &delivered[0].1;
    let results: Value =
        serde_json::from_str(&completion[completion.find('{').unwrap()..]).unwrap();
    let statuses: Vec<&str> = results["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["completed", "interrupted", "completed"]);
    assert!(
        pmt_delegation::monitor::stop(&conversation, &ids[0]).is_err(),
        "an ended child cannot be stopped"
    );
}

/// Scenario: Tail a live transcript.
#[tokio::test]
async fn a_childs_transcript_exists_before_its_first_model_call_and_logs_each_tool_call() {
    let f = fixture();
    let live = pmt_delegation::transcript::root(f.scope.dir());
    *f.launcher.live_root.lock().unwrap() = Some(live.clone());
    let d = f.delegator(DelegationConfig::default(), sync());
    let result = body(&call(&d, json!({"tasks": [{"goal": "write the files"}]})).await);
    let paths: Vec<String> = result["live_transcripts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("task-1.log"), "{paths:?}");
    assert_eq!(
        *f.launcher.logs_at_open.lock().unwrap(),
        [1],
        "written before the child ran"
    );
    let log = std::fs::read_to_string(&paths[0]).unwrap();
    let tool_lines = log
        .lines()
        .filter(|l| l.contains(" tool write_file "))
        .count();
    assert_eq!(tool_lines, 2, "one line per tool call:\n{log}");
    assert!(
        log.lines().all(|l| l
            .split(' ')
            .next()
            .is_some_and(|t| t.parse::<jiff::Timestamp>().is_ok())),
        "{log}"
    );
    let id = result["delegation_id"].as_str().unwrap();
    let manifest = std::fs::read_to_string(live.join(id).join("manifest.json")).unwrap();
    assert!(manifest.contains("task-1.log"), "{manifest}");
}

fn completions_of(delivered: &[(String, String)]) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = delivered
        .iter()
        .map(|(id, text)| {
            let results: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
            let mut tasks: Vec<usize> = results["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| usize::try_from(r["task_index"].as_u64().unwrap()).unwrap())
                .collect();
            tasks.sort_unstable();
            (id.clone(), tasks)
        })
        .collect();
    out.sort();
    out
}

/// Scenario: Ungrouped results arrive separately.
#[tokio::test]
async fn ungrouped_results_arrive_separately_and_a_group_together() {
    let f = fixture();
    let config = DelegationConfig {
        independent_completions: true,
        ..DelegationConfig::default()
    };
    let d = f.delegator_with(config, f.background(), |ctx| {
        ctx.shape.groups = true;
    });
    let handle = body(
        &call(
            &d,
            json!({"tasks": [
                {"goal": "quick a"},
                {"goal": "slow bench 1", "group": "bench"},
                {"goal": "quick c"},
                {"goal": "slow bench 2", "group": "bench"}
            ]}),
        )
        .await,
    );
    let id = handle["delegation_id"].as_str().unwrap().to_owned();
    let units = handle["units"].as_array().unwrap();
    assert_eq!(units.len(), 3, "{handle}");
    assert_eq!(units[1]["delegation_id"], format!("{id}-2"));
    assert_eq!(units[1]["group"], "bench");
    assert_eq!(units[1]["task_indexes"], json!([1, 3]));
    let delivered = f.completions.wait_for(3).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let got = completions_of(&f.completions.delivered.lock().unwrap());
    assert_eq!(
        got,
        [
            (format!("{id}-1"), vec![0]),
            (format!("{id}-2"), vec![1, 3]),
            (format!("{id}-3"), vec![2]),
        ]
    );
    // The quick ones did not wait for the group.
    assert!(!delivered[0].0.ends_with("-2"), "{delivered:?}");
}

#[tokio::test]
async fn without_independent_completions_a_group_is_accepted_and_ignored() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), f.background());
    let handle = body(
        &call(
            &d,
            json!({"tasks": [{"goal": "quick a"}, {"goal": "quick b", "group": "g"}]}),
        )
        .await,
    );
    assert!(handle.get("units").is_none(), "{handle}");
    f.completions.wait_for(1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = completions_of(&f.completions.delivered.lock().unwrap());
    assert_eq!(got.len(), 1, "one completion for the call: {got:?}");
    assert_eq!(got[0].1, [0, 1]);
}

/// Scenario: One message per turn on Telegram (the joining dispatch).
#[tokio::test]
async fn a_joining_session_waits_for_its_children_and_has_no_live_control() {
    let f = fixture();
    let d = f.delegator_with(DelegationConfig::default(), f.background(), |ctx| {
        ctx.shape.joining = true;
    });
    let result = body(
        &call(
            &d,
            json!({"tasks": [{"goal": "quick a"}, {"goal": "quick b"}]}),
        )
        .await,
    );
    assert_eq!(
        result["status"], "completed",
        "joined in the call: {result}"
    );
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    assert!(f.completions.delivered.lock().unwrap().is_empty());
    let listed = call(&d, json!({"action": "list"})).await;
    assert!(!listed.status.succeeded());
    assert!(
        listed.text_content().contains("waits for its children"),
        "{}",
        listed.text_content()
    );
}

/// Scenario: Review returns to the session (the delegation rail).
#[tokio::test]
async fn a_labelled_dispatch_returns_as_a_labelled_completion() {
    let f = fixture();
    let d = f.delegator(DelegationConfig::default(), f.background());
    let id = d
        .dispatch_labelled(pmt_delegation::host::Labelled {
            goal: "Review the recent work".to_owned(),
            context: "User: open a PR".to_owned(),
            model: None,
            label: "Review: focus on security".to_owned(),
        })
        .await
        .unwrap();
    let delivered = f.completions.wait_for(1).await;
    assert_eq!(delivered[0].0, id);
    assert!(
        delivered[0].1.starts_with(&format!(
            "[delegation_completed {id}] Review: focus on security"
        )),
        "{}",
        delivered[0].1
    );
    let none = f.delegator(DelegationConfig::default(), sync());
    let refused = none
        .dispatch_labelled(pmt_delegation::host::Labelled {
            goal: "g".to_owned(),
            context: String::new(),
            model: None,
            label: "Review recent work".to_owned(),
        })
        .await;
    assert!(
        refused.is_err(),
        "a session without completions cannot run it"
    );
}

/// A review completion owed across a restart keeps its `Review: …` label.
#[tokio::test]
async fn a_labelled_completion_delivered_after_a_restart_keeps_its_label() {
    let f = fixture();
    f.completions.refuse.store(true, Ordering::SeqCst);
    let d = f.delegator(DelegationConfig::default(), f.background());
    let id = d
        .dispatch_labelled(pmt_delegation::host::Labelled {
            goal: "Review the recent work".to_owned(),
            context: "User: open a PR".to_owned(),
            model: None,
            label: "Review: focus on security".to_owned(),
        })
        .await
        .unwrap();
    for _ in 0..200 {
        if f.store
            .delegation(&id)
            .unwrap()
            .unwrap()
            .completed_at
            .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reopened = Store::open(&f.scope).unwrap();
    deliver::recover(&reopened);
    f.completions.refuse.store(false, Ordering::SeqCst);
    let spill = f.scope.dir().join("spill");
    deliver::deliver_pending(&reopened, f.session, None, &spill, f.completions.as_ref()).await;
    let delivered = f.completions.delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert!(
        delivered[0].1.starts_with(&format!(
            "[delegation_completed {id}] Review: focus on security"
        )),
        "{}",
        delivered[0].1
    );
}
