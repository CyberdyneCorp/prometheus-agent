//! The `kanban` subcommands and their flags (spec: kanban, CLI surface).

use std::path::PathBuf;

use clap::{Args, Subcommand};

/// `prometheus kanban` arguments.
#[derive(Debug, Clone, Args)]
pub struct KanbanArgs {
    /// The board (default: the board of the project owning the current
    /// directory or the active project, else `kanban.current_board`, else
    /// `default`).
    #[arg(long, global = true, value_name = "SLUG")]
    pub board: Option<String>,
    /// Act as the worker of a run: its ticket line, or `-` to read it from
    /// stdin (external worker lanes and manual claims).
    #[arg(long, global = true, value_name = "TICKET")]
    pub ticket: Option<String>,
    /// The kanban subcommand.
    #[command(subcommand)]
    pub command: KanbanCommand,
}

/// `prometheus kanban` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum KanbanCommand {
    /// Create a task (ready, or todo while a parent is open).
    Create(Box<CreateArgs>),
    /// The board's tasks by priority then age.
    #[command(visible_alias = "ls")]
    List {
        /// Only this status.
        #[arg(long)]
        status: Option<String>,
        /// Only this assignee.
        #[arg(long)]
        assignee: Option<String>,
        /// Include archived tasks.
        #[arg(long)]
        all: bool,
    },
    /// One task: fields, links, comments, attachments, runs and spend.
    Show {
        /// The task.
        id: String,
    },
    /// Change a task's title, body, priority, reviewer or review lane.
    Edit {
        /// The task.
        id: String,
        /// New title.
        #[arg(long)]
        title: Option<String>,
        /// New body.
        #[arg(long)]
        body: Option<String>,
        /// New priority.
        #[arg(long, allow_hyphen_values = true)]
        priority: Option<i64>,
        /// New reviewer profile (`none` clears it).
        #[arg(long)]
        reviewer: Option<String>,
        /// Review lane: `on`, `off` or `board` (the board's default).
        #[arg(long, value_parser = ["on", "off", "board"])]
        review: Option<String>,
    },
    /// Assign tasks to a profile (`none` unassigns).
    Assign {
        /// The profile.
        profile: String,
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Make CHILD depend on PARENT.
    Link {
        /// The parent.
        parent: String,
        /// The child.
        child: String,
        /// The child's current run id (to link a running child).
        #[arg(long)]
        run: Option<String>,
    },
    /// Remove a dependency.
    Unlink {
        /// The parent.
        parent: String,
        /// The child.
        child: String,
    },
    /// Add a comment.
    Comment {
        /// The task.
        id: String,
        /// The text.
        body: String,
    },
    /// Attach files (at most `kanban.attachment_max_bytes` each).
    Attach {
        /// The task.
        id: String,
        /// The files.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// A task's attachments.
    Attachments {
        /// The task.
        id: String,
    },
    /// Block tasks with a typed reason.
    Block {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// `dependency`, `needs_input`, `capability`, `budget` or `transient`.
        #[arg(long)]
        kind: String,
        /// Why.
        #[arg(long)]
        reason: String,
    },
    /// Lift blocks and schedules.
    Unblock {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Park tasks until a time (RFC 3339), or until unblocked.
    Schedule {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// When they wake (RFC 3339).
        #[arg(long)]
        at: Option<String>,
    },
    /// Complete tasks through the completion gates.
    Complete {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// The result.
        #[arg(long)]
        result: Option<String>,
        /// The summary.
        #[arg(long)]
        summary: Option<String>,
        /// The open run a live worker owns (completes it for the worker).
        #[arg(long)]
        run: Option<String>,
        /// Complete despite a live worker.
        #[arg(long)]
        force: bool,
        /// Facts as a JSON object (`{"published_pr": "<url>"}` for a PR
        /// completion contract).
        #[arg(long)]
        metadata: Option<String>,
    },
    /// Hand tasks to review (from running or ready).
    RequestReview {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// The summary.
        #[arg(long)]
        summary: Option<String>,
        /// Facts as a JSON object (`published_pr` binds a PR contract).
        #[arg(long)]
        metadata: Option<String>,
        /// The reviewer profile.
        #[arg(long)]
        reviewer: Option<String>,
    },
    /// Send tasks in review back to ready (or todo while a parent is open).
    ReopenReview {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// Why (kept as a comment).
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reassign tasks to another profile or worker lane.
    #[command(override_usage = "prometheus kanban reassign <ID>... <PROFILE>")]
    Reassign {
        /// The tasks, then the profile.
        #[arg(required = true, num_args = 2..)]
        args: Vec<String>,
    },
    /// Set (or with `none` clear) a task's model override; it applies from
    /// the next dispatch.
    SetModel {
        /// The task.
        id: String,
        /// The model, or `none`.
        model: String,
        /// The model's provider.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Claim a ready task for an external worker: prints the run id and
    /// its ticket (pass it with `--ticket` to report).
    Claim {
        /// The task.
        id: String,
        /// Seconds the claim lasts without a heartbeat (default:
        /// `kanban.claim_ttl_secs`).
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Renew a claimed task's lease (with `--ticket`, or as the person who
    /// claimed it).
    Heartbeat {
        /// The task.
        id: String,
        /// A short note for the log.
        #[arg(long)]
        note: Option<String>,
    },
    /// Create the default board and print the dispatcher status.
    Init,
    /// Create a swarm: a root card holding the goal and blackboard, one
    /// card per worker profile, a verifier and a synthesizer.
    Swarm {
        /// The goal.
        goal: String,
        /// Worker profiles, comma separated.
        #[arg(long, value_delimiter = ',', required = true)]
        workers: Vec<String>,
        /// The verifier profile.
        #[arg(long)]
        verifier: String,
        /// The synthesizer profile.
        #[arg(long)]
        synthesizer: String,
    },
    /// Expand triage ideas into full bodies with acceptance criteria (the
    /// `kanban` auxiliary model) and move them to todo.
    Specify {
        /// The task.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<String>,
        /// Every triage task of the board.
        #[arg(long)]
        all: bool,
        /// Only tasks of this assignee (tenant).
        #[arg(long)]
        tenant: Option<String>,
    },
    /// Decompose a task into a validated child-task graph (the `kanban`
    /// auxiliary model), created in one transaction under it.
    Decompose {
        /// The task.
        id: String,
    },
    /// Print new board events as they happen.
    Watch {
        /// Only tasks of this assignee.
        #[arg(long)]
        assignee: Option<String>,
        /// Only these event kinds, comma separated.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
    },
    /// The assignees a task may name: the home's profiles and the worker
    /// lanes of enabled plugins.
    Assignees,
    /// Approve tasks in review.
    Approve {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// A comment.
        #[arg(long)]
        comment: Option<String>,
    },
    /// Send tasks in review back to todo with a comment.
    RequestChanges {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// What to change.
        #[arg(long)]
        comment: String,
    },
    /// Archive tasks (a running task's worker is terminated).
    Archive {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Restore archived tasks to triage, todo or ready.
    Restore {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// The status (default: ready or todo by the parents).
        #[arg(long, value_parser = ["triage", "todo", "ready"])]
        to: Option<String>,
    },
    /// Promote todo tasks whose parents are settled.
    Promote {
        /// The tasks (all todo tasks when none).
        ids: Vec<String>,
    },
    /// Print exactly the context a worker of the task would receive.
    Context {
        /// The task.
        id: String,
    },
    /// A task's event log.
    Log {
        /// The task.
        id: String,
    },
    /// A task's runs (attempts) with their sessions and spend.
    Runs {
        /// The task.
        id: String,
    },
    /// The board's latest events; `--follow` keeps printing new ones.
    Tail {
        /// Events shown first.
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
        /// Keep printing.
        #[arg(short, long)]
        follow: bool,
    },
    /// Counts by status and run outcome.
    Stats,
    /// The dispatcher lease, the last ticks and the open runs.
    Diag,
    /// Run the dispatcher: one tick (`--once`), a standalone loop, or a
    /// dry run that changes nothing.
    Dispatch {
        /// One tick.
        #[arg(long, conflicts_with_all = ["looping", "dry_run"])]
        once: bool,
        /// Tick every `kanban.dispatch_interval_secs` until stopped.
        #[arg(long = "loop", conflicts_with = "dry_run")]
        looping: bool,
        /// Print what a tick would do (`Guarded (<reason>): <id>`...).
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a claimed task (started by the dispatcher with its run ticket
    /// on stdin).
    #[command(hide = true)]
    Work {
        /// The run.
        #[arg(long)]
        run: String,
    },
    /// Broker a claimed lane task: run its plugin's sandboxed lane and
    /// apply its reports (started by the dispatcher with the run ticket on
    /// stdin).
    #[command(hide = true)]
    Lane {
        /// The run.
        #[arg(long)]
        run: String,
    },
    /// Notification subscriptions.
    Notify {
        /// The notify subcommand.
        #[command(subcommand)]
        command: NotifyCommand,
    },
    /// Purge old subscriptions, tick summaries and leftover scratch
    /// workspaces.
    Gc,
    /// Boards: list, create, rename, archive, switch, export, import.
    Boards {
        /// The boards subcommand.
        #[command(subcommand)]
        command: BoardsCommand,
    },
}

/// `kanban create` flags.
#[derive(Debug, Clone, Args)]
pub struct CreateArgs {
    /// The title.
    pub title: String,
    /// The body (instructions, acceptance criteria).
    #[arg(long)]
    pub body: Option<String>,
    /// The worker profile.
    #[arg(long)]
    pub assignee: Option<String>,
    /// The reviewer profile.
    #[arg(long)]
    pub reviewer: Option<String>,
    /// A parent task (repeatable).
    #[arg(long = "parent")]
    pub parents: Vec<String>,
    /// scratch, worktree[:<branch>] or dir:<absolute path>.
    #[arg(long, default_value = "scratch")]
    pub workspace: String,
    /// Higher is claimed first.
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    pub priority: i64,
    /// triage, todo or blocked (default: from the parents).
    #[arg(long = "status", value_parser = ["triage", "todo", "blocked"])]
    pub initial_status: Option<String>,
    /// Return the live task with this key instead of a duplicate.
    #[arg(long)]
    pub idempotency_key: Option<String>,
    /// Send worker completions to review.
    #[arg(long, conflicts_with = "no_review")]
    pub review: bool,
    /// Never send worker completions to review.
    #[arg(long)]
    pub no_review: bool,
    /// Failed runs before the breaker trips.
    #[arg(long)]
    pub max_retries: Option<u32>,
    /// USD limit across every run.
    #[arg(long)]
    pub budget_usd: Option<f64>,
    /// Token limit across every run.
    #[arg(long)]
    pub budget_tokens: Option<u64>,
    /// A file the worker must leave in its workspace (repeatable).
    #[arg(long = "artifact")]
    pub artifacts: Vec<String>,
    /// agent, eval or evolve.
    #[arg(long, default_value = "agent", value_parser = ["agent", "eval", "evolve"])]
    pub kind: String,
    /// The eval suite (kind eval).
    #[arg(long)]
    pub suite: Option<String>,
    /// live or replay (kind eval).
    #[arg(long, value_parser = ["live", "replay"])]
    pub provider_mode: Option<String>,
    /// The evolution target task (kind evolve).
    #[arg(long)]
    pub target_task: Option<String>,
    /// parallel or sequential (kind evolve).
    #[arg(long, value_parser = ["parallel", "sequential"], default_value = "parallel")]
    pub mode: String,
    /// Candidates per round (kind evolve).
    #[arg(long, default_value_t = 2)]
    pub budget_k: u32,
    /// `OWNER/REPO`, a pull-request URL or `local-only` (the default).
    #[arg(long)]
    pub completion_contract: Option<String>,
    /// Run the worker in the goal loop, judged after each turn.
    #[arg(long)]
    pub goal: bool,
    /// Judged turns of the goal loop (default 20).
    #[arg(long, requires = "goal")]
    pub goal_max_turns: Option<u32>,
    /// The worker's model instead of the assignee's default.
    #[arg(long)]
    pub model: Option<String>,
    /// The model's provider (needs `--model`).
    #[arg(long, requires = "model")]
    pub provider: Option<String>,
    /// A skill preloaded in the worker session (repeatable).
    #[arg(long = "skill")]
    pub skills: Vec<String>,
}

/// `kanban notify` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum NotifyCommand {
    /// Report a task's `completed`, `blocked`, `gave_up` and `review_requested`
    /// events to a chat, through the subscriber profile's adapter.
    Subscribe {
        /// The task.
        id: String,
        /// The adapter (`telegram`, `slack`...).
        #[arg(long)]
        adapter: String,
        /// The chat id.
        #[arg(long)]
        chat: String,
        /// The thread id.
        #[arg(long)]
        thread: Option<String>,
        /// `notify` (a message), `notify+wake` (and a turn of the
        /// subscriber's agent in that chat) or `wake` (the turn only).
        #[arg(long, default_value = "notify", value_parser = ["notify", "notify+wake", "wake"])]
        delivery_mode: String,
        /// `dm`, `group`, `channel` or `thread`: how a wake finds the chat's
        /// session.
        #[arg(long, value_parser = ["dm", "group", "channel", "thread"])]
        chat_type: Option<String>,
    },
    /// Subscriptions of a task, or of the board.
    List {
        /// The task.
        id: Option<String>,
    },
    /// Remove a subscription.
    Unsubscribe {
        /// The subscription id.
        id: String,
    },
}

/// `kanban boards` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum BoardsCommand {
    /// Every board, archived ones included.
    List,
    /// Create a board.
    Create {
        /// The slug.
        slug: String,
        /// A display name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Rename a board's slug.
    Rename {
        /// The board.
        from: String,
        /// The new slug.
        to: String,
    },
    /// Archive a board (it can no longer be selected).
    Archive {
        /// The board.
        slug: String,
    },
    /// Make a board this profile's current board (`kanban.current_board`).
    Switch {
        /// The board.
        slug: String,
    },
    /// Write a board's tasks, links, events, runs and attachments to FILE.
    Export {
        /// The board.
        slug: String,
        /// The archive.
        file: PathBuf,
    },
    /// Restore an exported board as a new board.
    Import {
        /// The archive.
        file: PathBuf,
        /// The new board's slug (needed when the exported slug is taken).
        #[arg(long)]
        rename: Option<String>,
    },
}
