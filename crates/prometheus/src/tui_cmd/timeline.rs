//! The session timeline in the TUI (spec: tui, Session timeline
//! scrubber): `/timeline` or `Ctrl+T` lists the shown session's turns
//! (`session.timeline`); moving the cursor shows the highlighted turn
//! read-only in place of the transcript, the live session untouched.
//! `Enter` rewinds there: `session.rewind` makes a new branch session
//! ending at that turn, which becomes the shown one (the original stays
//! live and listed), and when later turns changed files the TUI offers to
//! restore the workspace to that turn's checkpoint (`files.rollback`).

use crossterm::event::{KeyCode, KeyEvent};
use pmt_core::error::Error;
use pmt_i18n::t;
use pmt_rpc::contracts::{
    FilesRollback, FilesRollbackParams, RewindParams, SessionRef, SessionRewind, SessionRewound,
    SessionTimeline, SessionTranscript, Timeline as TurnList, Transcript,
};
use pmt_tui::app::{Card, Overlay};
use pmt_tui::timeline::{Timeline, Turn};
use pmt_tui::transcript::{Kind, Transcript as Shown};

use super::link::resume;
use super::overlays::Step;
use super::screen::{Done, Tui};

/// The workspace restore a rewind offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Restore {
    /// The branch session (its workspace).
    session: String,
    /// The `files.rollback` checkpoint number.
    checkpoint: u32,
    /// The turn the branch ends at.
    turn: u32,
}

fn rpc(err: &pmt_rpc::RpcError) -> Error {
    pmt_rpc::client::error_of(err)
}

impl Tui<'_> {
    /// `/timeline`, `Ctrl+T`: asks for the turns and the transcript the
    /// previews come from.
    pub(super) fn open_timeline(&self) {
        let params = SessionRef {
            session_id: self.link.session.clone(),
        };
        let client = self.link.client.clone();
        self.background(async move {
            let turns = client.call_typed::<SessionTimeline>(&params).await;
            let transcript = client.call_typed::<SessionTranscript>(&params).await;
            Done::Timeline(
                turns
                    .and_then(|t| transcript.map(|tr| (t, tr)))
                    .map_err(|e| rpc(&e)),
            )
        });
    }

    /// The turns arrived: the overlay opens on the newest, previewed.
    pub(super) fn timeline_loaded(&mut self, loaded: Result<(TurnList, Transcript), Error>) {
        let (list, transcript) = match loaded {
            Ok(loaded) => loaded,
            Err(err) => return self.error(&err),
        };
        if list.turns.is_empty() {
            let empty = self.strings().get("tui.timeline_empty").to_owned();
            return self.notice(empty);
        }
        self.previews = previews(&list, &transcript);
        let turns = list
            .turns
            .iter()
            .map(|t| Turn {
                number: t.turn,
                at: clock(&t.at),
                first_line: t.first_line.clone(),
                tools: t.tools,
                cost_usd: t.cost_usd,
            })
            .collect();
        let timeline = Timeline::new(turns);
        self.preview(&timeline);
        self.app.overlay = Some(Overlay::Timeline(timeline));
    }

    /// Shows the highlighted turn read-only.
    fn preview(&mut self, timeline: &Timeline) {
        let Some(turn) = timeline.selected() else {
            return;
        };
        let heading = t!(self.strings(), "tui.timeline_preview", 0 = turn.number);
        let shown = self
            .previews
            .get(timeline.selected)
            .cloned()
            .unwrap_or_default();
        self.app.preview = Some((heading, shown));
    }

    pub(super) fn timeline_key(&mut self, timeline: &mut Timeline, key: &KeyEvent) -> Step {
        match key.code {
            KeyCode::Esc => return Step::Close,
            KeyCode::Up => timeline.move_selection(-1),
            KeyCode::Down => timeline.move_selection(1),
            KeyCode::PageUp => timeline.move_selection(-10),
            KeyCode::PageDown => timeline.move_selection(10),
            KeyCode::Enter => {
                if let Some(turn) = timeline.selected() {
                    self.rewind(turn.number);
                }
                return Step::Close;
            }
            _ => return Step::Stay,
        }
        self.preview(timeline);
        Step::Stay
    }

    /// `rewind here`.
    fn rewind(&self, to_turn: u32) {
        let params = RewindParams {
            session_id: self.link.session.clone(),
            to_turn,
        };
        let client = self.link.client.clone();
        self.background(async move {
            Done::Rewound(
                client
                    .call_typed::<SessionRewind>(&params)
                    .await
                    .map_err(|e| rpc(&e)),
            )
        });
    }

    /// The branch exists: it becomes the shown session (the original
    /// stays live), with the workspace restore offered when there is one.
    pub(super) fn rewound(&mut self, rewound: Result<SessionRewound, Error>) {
        let rewound = match rewound {
            Ok(rewound) => rewound,
            Err(err) => return self.error(&err),
        };
        let line = t!(
            self.strings(),
            "tui.rewound",
            0 = rewound.turns,
            1 = rewound.session_id
        );
        self.notice(line);
        let restore = rewound.checkpoint.map(|checkpoint| Restore {
            session: rewound.session_id.clone(),
            checkpoint,
            turn: rewound.turns,
        });
        let client = self.link.client.clone();
        let branch = rewound.session_id;
        self.background(async move {
            let attached = resume(&client, &branch, None).await;
            Done::Branched(attached.map(|a| (branch, a)), restore)
        });
    }

    /// The restore offer's card.
    pub(super) fn restore_card(&self) -> Option<Card> {
        let restore = self.restore.as_ref()?;
        let strings = self.strings();
        Some(Card {
            title: t!(strings, "tui.restore_title", 0 = restore.turn),
            hint: strings.get("tui.restore_hint").to_owned(),
            ..Card::default()
        })
    }

    /// `y` restores the checkpoint, `n` keeps the files; `false` for any
    /// other line (sent as usual, the card stays).
    pub(super) fn answer_restore(&mut self, line: &str) -> bool {
        let yes = match line.trim().to_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => return false,
        };
        let Some(restore) = self.restore.take() else {
            return false;
        };
        if yes {
            let params = FilesRollbackParams {
                session_id: restore.session,
                checkpoint: Some(restore.checkpoint),
                file: None,
                diff: false,
                all: false,
            };
            let client = self.link.client.clone();
            self.background(async move {
                Done::Restored(
                    client
                        .call_typed::<FilesRollback>(&params)
                        .await
                        .map(|r| r.output)
                        .map_err(|e| rpc(&e)),
                )
            });
        }
        self.show_question();
        true
    }
}

/// `HH:MM:SS` of an RFC 3339 time, in the local time zone.
fn clock(at: &str) -> String {
    at.parse::<jiff::Timestamp>().map_or_else(
        |_| at.to_owned(),
        |t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%H:%M:%S")
                .to_string()
        },
    )
}

/// Each turn of `list` as a read-only transcript, from the session's
/// active messages.
fn previews(list: &TurnList, transcript: &Transcript) -> Vec<Shown> {
    let messages = &transcript.messages;
    let start = |id: &str| messages.iter().position(|m| m.id == id);
    list.turns
        .iter()
        .enumerate()
        .map(|(n, turn)| {
            let from = start(&turn.message_id).unwrap_or(messages.len());
            let to = list
                .turns
                .get(n + 1)
                .and_then(|next| start(&next.message_id))
                .unwrap_or(messages.len());
            let mut shown = Shown::default();
            for message in messages.get(from..to).unwrap_or_default() {
                let text = message.text.clone().unwrap_or_default();
                let kind = match message.role.as_str() {
                    "user" => Kind::User,
                    "assistant" if !text.is_empty() => Kind::Assistant,
                    "tool" => Kind::Tool,
                    _ => continue,
                };
                let text = if kind == Kind::Tool {
                    text.lines().next().unwrap_or_default().to_owned()
                } else {
                    text
                };
                shown.push(kind, text);
            }
            shown
        })
        .collect()
}
