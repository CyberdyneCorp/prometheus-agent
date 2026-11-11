//! Running a user turn in the line interface (spec: cli, Busy policy
//! enforcement; Interactive input handling: Ctrl+C interrupts the turn,
//! not the process). While the turn runs the REPL keeps reading: a typed
//! slash command follows its busy policy and is never sent as user text;
//! plain text follows `busy.input_mode`; prompts are answered from the
//! next line.

use std::sync::Arc;

use pmt_agent::turn::{ExitReason, Interrupt, Issuer, TurnInput, TurnOptions, TurnResult};
use pmt_config::schema::BusyInputMode;
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_core::human::Surface as Prompts;
use pmt_core::secret::SecretString;

use super::input::Event;
use super::prompts::{self, Pending};
use super::{Flow, Repl};

/// What a turn of the line interface runs.
pub(super) enum Work {
    /// A new turn of this input.
    Input(TurnInput),
    /// A turn a crash left unfinished: resumed (`true`) or closed.
    Resume(pmt_agent::turn::Unfinished, bool),
}
use crate::chat::session::Live;
use crate::chat::theme::Role;
use crate::slash::{BusyPolicy, Resolution, Surface};

impl Repl {
    /// Sends `text` as a user turn, with the pending attachments and
    /// `!!` context.
    pub(super) async fn user_turn(&mut self, text: String) {
        let mut input = match self.compose_input(text).await {
            Ok(input) => input,
            Err(err) => {
                self.error(&err);
                return;
            }
        };
        input.additions.append(&mut self.shell_context);
        self.run_input(input).await;
    }

    /// Runs one turn of `input` to its end, then shows the outcome.
    pub(super) async fn run_input(&mut self, input: TurnInput) {
        self.run_work(Work::Input(input)).await;
    }

    /// Runs a new turn, or resumes (or closes) one a crash left
    /// unfinished, to its end, then shows the outcome.
    pub(super) async fn run_work(&mut self, work: Work) {
        let Some(live) = self.live.take() else {
            return;
        };
        let origin = match &work {
            Work::Input(input) => input.origin.clone(),
            Work::Resume(..) => None,
        };
        if let Some(pause) = pmt_core::pause::active(live.scope.home()) {
            self.say(Role::Error, &pause.notice(&*self.strings()));
            self.live = Some(live);
            return;
        }
        let work = match work {
            Work::Input(mut input) => {
                if let Some(note) = live.skills.change_note() {
                    input.additions.push(note);
                }
                Work::Input(input)
            }
            resume @ Work::Resume(..) => resume,
        };
        live.record.turn();
        let mut options = TurnOptions::new(&CancelToken::new());
        options.run_budget = self.start.run_budget;
        if let Some(mouth) = &self.mouth {
            options.deltas = Arc::new(super::voice::Spoken(Arc::clone(mouth)));
        }
        let interrupt = options.interrupt.clone();
        let prompt = self.prompt();
        self.input.request(&prompt);
        self.input.set_busy(true);
        self.running = true;
        let started = std::time::Instant::now();
        let result = {
            // Boxed: the turn's state machine is large.
            let turn = Box::pin(async {
                match work {
                    Work::Input(input) => {
                        live.agent
                            .run_turn(Some(&live.scope), live.id, input, options)
                            .await
                    }
                    Work::Resume(unfinished, resume) => {
                        live.agent
                            .resume_turn(Some(&live.scope), unfinished, resume, options)
                            .await
                    }
                }
            });
            Box::pin(self.busy(turn, &live, &interrupt)).await
        };
        self.input.set_busy(false);
        self.running = false;
        let calls = self.end_progress();
        let outcome = super::automation::Outcome::of(origin.as_deref(), &result);
        self.after_turn(&live, result, started.elapsed(), calls)
            .await;
        self.automation_after(&live, outcome).await;
        if let Some(plan) = live.tools.end_plan() {
            self.notice(&crate::chat::commands::planning::finished(
                &self.strings(),
                &plan,
            ));
        }
        if let Some(back) = live.model.end_turn() {
            let route = format!(
                "{}/{}",
                back.provider,
                back.model.as_deref().unwrap_or("default")
            );
            self.notice(&pmt_i18n::t!(
                self.strings(),
                "display.model_back",
                route = route
            ));
        }
        self.spacer();
        self.live = Some(live);
    }

    /// Awaits `turn` while serving input and prompts.
    async fn busy<F>(
        &mut self,
        turn: F,
        live: &Live,
        interrupt: &Interrupt,
    ) -> Result<TurnResult, Error>
    where
        F: std::future::Future<Output = Result<TurnResult, Error>>,
    {
        let mut turn = std::pin::pin!(turn);
        loop {
            tokio::select! {
                result = &mut turn => return result,
                event = self.input.next() => {
                    self.busy_event(event, live, interrupt).await;
                    let prompt = self.prompt();
                    self.input.request(&prompt);
                }
                Some(prompt) = next(&mut self.approvals) => {
                    self.ring_bell();
                    let question = prompts::approval_question(prompt.request(), &self.strings());
                    self.say(Role::Notice, &question);
                    self.ask(Pending::Approval(prompt));
                }
                Some(prompt) = next(&mut self.clarify) => {
                    self.ring_bell();
                    if let Some(question) = prompts::clarify_question(prompt.request(), 0) {
                        self.say(Role::Notice, &question);
                    }
                    self.ask(Pending::Clarify { prompt, answers: Vec::new() });
                }
                Some(text) = self.to_post.recv() => self.notice(&text),
                Some(progress) = recv(&mut self.progress) => self.on_progress(progress),
                Some(ask) = self.memory.recv() => {
                    self.say(Role::Notice, &ask.preview);
                    self.ask(Pending::Memory(ask.reply));
                }
                Some(ask) = self.installs.recv() => {
                    self.ring_bell();
                    self.say(Role::Notice, &ask.question);
                    self.ask(Pending::Install(ask.reply));
                }
                Some(ask) = self.secrets.recv() => {
                    self.ring_bell();
                    self.say(Role::Notice, &ask.question);
                    self.input.set_masked(true);
                    self.ask(Pending::Secret(ask.reply));
                }
            }
        }
    }

    /// Makes `pending` the question the next line answers (refreshing the
    /// prompt that is being read).
    fn ask(&mut self, pending: Pending) {
        if let Some(old) = self.pending.replace(pending) {
            old.cancel();
        }
        let strings = self.strings();
        self.say(
            Role::Prompt,
            &self
                .pending
                .as_ref()
                .map(|p| p.prompt(&strings))
                .unwrap_or_default(),
        );
    }

    pub(super) fn ring_bell(&self) {
        if self.factory.scope.config().display.bell_on_prompt {
            self.input.print("\x07");
        }
    }

    async fn busy_event(&mut self, event: Event, session: &Live, interrupt: &Interrupt) {
        match event {
            Event::Interrupt | Event::Eof => {
                if let Some(pending) = self.pending.take() {
                    self.input.set_masked(false);
                    pending.cancel();
                }
                interrupt.stop(Issuer::User);
                if event == Event::Eof {
                    self.queue.push_back("/exit".to_owned());
                }
            }
            Event::SuspendRefused => self.notice(self.strings().get("cli.cannot_suspend")),
            // Voice records between turns only.
            Event::Record => {}
            Event::Compose(_) => {
                self.notice(self.strings().get("cli.prompt_busy"));
            }
            Event::Line(text) => {
                if let Some(pending) = self.pending.take() {
                    self.answer_now(pending, &text);
                } else if text.trim_start().starts_with('!') {
                    self.notice(self.strings().get("cli.shell_busy"));
                } else if text.trim_start().starts_with('/') {
                    self.busy_slash(text.trim(), session, interrupt).await;
                } else if !text.trim().is_empty() {
                    self.busy_text(text, session, interrupt);
                }
            }
        }
    }

    /// A slash command typed while the turn runs, by its busy policy.
    async fn busy_slash(&mut self, typed: &str, session: &Live, interrupt: &Interrupt) {
        let (name, policy) = match self.registry.resolve(typed, Surface::Cli) {
            Resolution::Builtin { command, .. } => (command.name.to_owned(), command.busy),
            Resolution::External { command, .. } => (command.name.clone(), command.busy),
            other => {
                if let Some(message) = other.message() {
                    self.notice(&message);
                }
                return;
            }
        };
        match policy {
            BusyPolicy::Reject => {
                self.notice(&pmt_i18n::t!(
                    self.strings(),
                    "cli.command_busy",
                    name = name
                ));
            }
            BusyPolicy::Queue => {
                self.queue.push_back(typed.to_owned());
                self.notice(&pmt_i18n::t!(
                    self.strings(),
                    "cli.command_queued",
                    name = name
                ));
            }
            BusyPolicy::InterruptThenDispatch => {
                interrupt.stop(Issuer::User);
                self.queue.push_front(typed.to_owned());
            }
            BusyPolicy::Immediate => {
                if let Resolution::Builtin { command, args } =
                    self.registry.resolve(typed, Surface::Cli)
                {
                    self.immediate(command.name, &args, session).await;
                }
            }
        }
    }

    /// Plain text typed while the turn runs, by `busy.input_mode`.
    fn busy_text(&mut self, text: String, live: &Live, interrupt: &Interrupt) {
        match self.factory.scope.config().busy.input_mode {
            BusyInputMode::Queue => {
                self.queue.push_back(text);
                self.notice(self.strings().get("cli.text_queued"));
            }
            BusyInputMode::Steer => match live.agent.steer(&text) {
                Ok(()) => self.notice(self.strings().get("cli.steering")),
                Err(err) => self.error(&err),
            },
            BusyInputMode::Interrupt => {
                interrupt.stop(Issuer::User);
                self.queue.push_front(text);
            }
        }
    }

    /// Answers an approval, clarify or memory prompt during a turn.
    pub(super) fn answer_now(&mut self, pending: Pending, line: &str) {
        match pending {
            Pending::Approval(prompt) => {
                let strings = self.strings();
                if let Some(decision) = prompts::decision(line) {
                    if let Some(outcome) = prompts::outcome(decision, &strings) {
                        self.notice(outcome);
                    }
                    prompt.respond(decision);
                } else {
                    self.notice(strings.get("approval.answer_hint"));
                    self.pending = Some(Pending::Approval(prompt));
                }
            }
            Pending::Clarify {
                prompt,
                mut answers,
            } => {
                let index = answers.len();
                answers.push(prompts::clarify_answer(prompt.request(), index, line));
                match prompts::clarify_question(prompt.request(), index + 1) {
                    Some(question) => {
                        self.say(Role::Notice, &question);
                        self.pending = Some(Pending::Clarify { prompt, answers });
                    }
                    None => prompt.respond(pmt_agent::tools::clarify::ClarifyAnswer { answers }),
                }
            }
            Pending::Memory(reply) => {
                let yes = matches!(line.trim().to_lowercase().as_str(), "y" | "yes");
                let _ = reply.send(yes);
            }
            Pending::Install(reply) => {
                let _ = reply.send(prompts::install_answer(line));
            }
            Pending::Secret(reply) => {
                self.input.set_masked(false);
                let value = (!line.is_empty()).then(|| SecretString::new(line));
                let _ = reply.send(value);
            }
            Pending::Confirm { .. } | Pending::Resume(_) => {}
            Pending::Autoprune => {
                let answer = super::super::autoprune::Answer::parse(line)
                    .unwrap_or(super::super::autoprune::Answer::Later);
                let store = &self.factory.store;
                match super::super::autoprune::apply(
                    &self.factory.scope,
                    store,
                    answer,
                    jiff::Timestamp::now(),
                ) {
                    Ok(text) => self.notice(&text),
                    Err(err) => self.error(&err),
                }
            }
        }
    }

    /// Shows a finished turn.
    async fn after_turn(
        &mut self,
        live: &Live,
        result: Result<TurnResult, Error>,
        elapsed: std::time::Duration,
        calls: u32,
    ) {
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                self.error(&err);
                return;
            }
        };
        for notice in &result.notices {
            self.notice(notice);
        }
        if let Some(answer) = &result.final_response {
            self.final_answer(answer);
        }
        // Before any stop notice or error, so those are the last lines.
        self.turn_summary(elapsed, calls, &result.usage);
        match result.exit_reason {
            ExitReason::Completed => {}
            ExitReason::InterruptedByUser => self.notice(self.strings().get("display.interrupted")),
            other => {
                let text = pmt_i18n::t!(
                    self.strings(),
                    "display.turn_ended",
                    reason = other.as_str()
                );
                self.notice(&text);
            }
        }
        if let Some(err) = &result.error {
            self.error(err);
        }
        crate::chat::reviews::after_turn(&self.factory, live, &result).await;
        if self.factory.scope.config().display.bell_on_complete {
            self.input.print("\x07");
        }
    }

    /// Answers a pending prompt from an idle line; a confirmation runs or
    /// cancels its command.
    pub(super) async fn answer(&mut self, pending: Pending, line: &str) -> Flow {
        match pending {
            Pending::Confirm { line: command } => Box::pin(self.confirmed(&command, line)).await,
            Pending::Resume(unfinished) => {
                let yes = matches!(line.trim().to_lowercase().as_str(), "y" | "yes");
                self.run_work(Work::Resume(unfinished, yes)).await;
                Flow::Continue
            }
            other => {
                self.answer_now(other, line);
                Flow::Continue
            }
        }
    }
}

/// The next prompt of `prompts`; never resolves when there is none.
pub(super) async fn next<Q, A>(
    prompts: &mut Option<Prompts<Q, A>>,
) -> Option<pmt_core::human::Prompt<Q, A>> {
    match prompts {
        Some(prompts) => prompts.next().await,
        None => std::future::pending().await,
    }
}

/// The next item of `rx`; never resolves when there is none.
async fn recv<T>(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

impl Repl {
    /// A resumed session whose last turn a crash left unfinished: resumed,
    /// closed or offered, by `agent.resume_on_crash` (spec:
    /// agent-turn-loop, Crash resume mid-tool-batch).
    pub(super) async fn offer_resume(&mut self) {
        use pmt_config::schema::ResumeOnCrash;
        let Some(live) = &self.live else {
            return;
        };
        let unfinished = match live.agent.unfinished_turn() {
            Ok(Some(unfinished)) => unfinished,
            Ok(None) => return,
            Err(err) => return self.error(&err),
        };
        let tools = unfinished.interrupted_tools();
        let strings = self.strings();
        let text = if tools.is_empty() {
            pmt_i18n::t!(strings, "display.unfinished")
        } else {
            let tools = tools.join(", ");
            pmt_i18n::t!(strings, "display.unfinished_tools", tools = tools)
        };
        self.notice(&text);
        match self.factory.scope.config().agent.resume_on_crash {
            ResumeOnCrash::Auto => self.run_work(Work::Resume(unfinished, true)).await,
            ResumeOnCrash::Never => self.run_work(Work::Resume(unfinished, false)).await,
            ResumeOnCrash::Ask => self.ask(Pending::Resume(unfinished)),
        }
    }

    /// The low-disk prune prompt, when it is due (spec: session-storage,
    /// Low-disk prune prompt).
    pub(super) fn offer_autoprune(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let due = super::super::autoprune::due(
            &self.factory.scope,
            &self.factory.store,
            jiff::Timestamp::now(),
        );
        match due {
            Ok(true) => {
                self.notice(&super::super::autoprune::question(&self.factory.scope));
                self.ask(Pending::Autoprune);
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(error = %err, "low-disk check skipped"),
        }
    }
}
