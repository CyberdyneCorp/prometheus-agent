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

use super::input::Event;
use super::prompts::{self, Pending};
use super::{Flow, Repl};
use crate::chat::session::Live;
use crate::chat::theme::Role;
use crate::slash::{BusyPolicy, Resolution, Surface};

impl Repl {
    /// Sends `text` as a user turn.
    pub(super) async fn user_turn(&mut self, text: String) {
        self.run_input(TurnInput::text(text)).await;
    }

    /// Runs one turn of `input` to its end, then shows the outcome.
    pub(super) async fn run_input(&mut self, mut input: TurnInput) {
        let Some(live) = self.live.take() else {
            return;
        };
        if let Some(note) = live.skills.change_note() {
            input.additions.push(note);
        }
        let mut options = TurnOptions::new(&CancelToken::new());
        options.run_budget = self.start.run_budget;
        let interrupt = options.interrupt.clone();
        let prompt = self.prompt();
        self.input.request(&prompt);
        let result = {
            // Boxed: the turn's state machine is large.
            let turn = Box::pin(
                live.agent
                    .run_turn(Some(&live.scope), live.id, input, options),
            );
            Box::pin(self.busy(turn, &live, &interrupt)).await
        };
        self.after_turn(&live, result);
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
                    self.say(Role::Notice, &prompts::approval_question(prompt.request()));
                    self.ask(Pending::Approval(prompt));
                }
                Some(prompt) = next(&mut self.clarify) => {
                    self.ring_bell();
                    if let Some(question) = prompts::clarify_question(prompt.request(), 0) {
                        self.say(Role::Notice, &question);
                    }
                    self.ask(Pending::Clarify { prompt, answers: Vec::new() });
                }
                Some(ask) = self.memory.recv() => {
                    self.say(Role::Notice, &ask.preview);
                    self.ask(Pending::Memory(ask.reply));
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
        self.say(
            Role::Prompt,
            &self
                .pending
                .as_ref()
                .map(Pending::prompt)
                .unwrap_or_default(),
        );
    }

    fn ring_bell(&self) {
        if self.factory.scope.config().display.bell_on_prompt {
            self.input.print("\x07");
        }
    }

    async fn busy_event(&mut self, event: Event, session: &Live, interrupt: &Interrupt) {
        match event {
            Event::Interrupt | Event::Eof => {
                if let Some(pending) = self.pending.take() {
                    pending.cancel();
                }
                interrupt.stop(Issuer::User);
                if event == Event::Eof {
                    self.queue.push_back("/exit".to_owned());
                }
            }
            Event::Line(text) => {
                if let Some(pending) = self.pending.take() {
                    self.answer_now(pending, &text);
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
            BusyPolicy::Reject => self.notice(&format!(
                "/{name} is unavailable while the agent is working; use /stop first"
            )),
            BusyPolicy::Queue => {
                self.queue.push_back(typed.to_owned());
                self.notice(&format!("/{name} queued; it runs after this turn"));
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
                self.notice("queued; it is sent when this turn ends");
            }
            BusyInputMode::Steer => match live.agent.steer(&text) {
                Ok(()) => self.notice("steering the running turn"),
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
                if let Some(decision) = prompts::decision(line) {
                    prompt.respond(decision);
                } else {
                    self.notice("answer o(nce), s(ession), a(lways) or d(eny)");
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
            Pending::Confirm { .. } => {}
        }
    }

    /// Shows a finished turn.
    fn after_turn(&mut self, live: &Live, result: Result<TurnResult, Error>) {
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
            self.say(Role::Assistant, answer);
        }
        match result.exit_reason {
            ExitReason::Completed => {}
            ExitReason::InterruptedByUser => self.notice("interrupted"),
            other => self.notice(&format!("turn ended: {}", other.as_str())),
        }
        if let Some(err) = &result.error {
            self.error(err);
        }
        if live.memory.end_turn() {
            self.spawn_memory_review(live);
        }
        if self.factory.scope.config().display.bell_on_complete {
            self.input.print("\x07");
        }
    }

    /// Runs the background memory review the session is due for.
    fn spawn_memory_review(&self, live: &Live) {
        let store = Arc::clone(&self.factory.store);
        let child = match pmt_agent::review::child_session(&store, live.id) {
            Ok(child) => child,
            Err(err) => {
                tracing::warn!(error = %err, "memory review skipped");
                return;
            }
        };
        let review = pmt_memory::review::ReviewRun {
            parent: Arc::clone(&live.memory),
            session: live.id,
            child,
            model: Arc::clone(&live.model) as _,
        };
        tokio::spawn(async move {
            if let Err(err) = pmt_memory::review::run(review).await {
                tracing::warn!(error = %err, "memory review failed");
            }
        });
    }

    /// Answers a pending prompt from an idle line; a confirmation runs or
    /// cancels its command.
    pub(super) async fn answer(&mut self, pending: Pending, line: &str) -> Flow {
        match pending {
            Pending::Confirm { line: command } => self.confirmed(&command, line).await,
            other => {
                self.answer_now(other, line);
                Flow::Continue
            }
        }
    }
}

/// The next prompt of `prompts`; never resolves when there is none.
async fn next<Q, A>(prompts: &mut Option<Prompts<Q, A>>) -> Option<pmt_core::human::Prompt<Q, A>> {
    match prompts {
        Some(prompts) => prompts.next().await,
        None => std::future::pending().await,
    }
}
