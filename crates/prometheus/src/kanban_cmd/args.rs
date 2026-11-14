//! The `kanban` subcommands and their flags (spec: kanban, CLI surface).

use std::path::PathBuf;

use clap::{Args, Subcommand};

/// `prometheus kanban` arguments.
#[derive(Debug, Clone, Args)]
pub struct KanbanArgs {
    /// The board (default: `kanban.current_board`, else `default`).
    #[arg(long, global = true, value_name = "SLUG")]
    pub board: Option<String>,
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
    },
    /// Hand tasks to review.
    RequestReview {
        /// The tasks.
        #[arg(required = true)]
        ids: Vec<String>,
        /// The summary.
        #[arg(long)]
        summary: Option<String>,
    },
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
    /// Run the dispatcher: one tick (`--once`) or a standalone loop.
    Dispatch {
        /// One tick.
        #[arg(long, conflicts_with = "looping")]
        once: bool,
        /// Tick every `kanban.dispatch_interval_secs` until stopped.
        #[arg(long = "loop")]
        looping: bool,
    },
    /// Run a claimed task (started by the dispatcher with its run ticket
    /// on stdin).
    #[command(hide = true)]
    Work {
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
