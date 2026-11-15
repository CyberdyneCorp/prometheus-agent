//! Lifecycle observers (spec: plugin-system, Kanban lifecycle observer
//! hooks). A board with an observer hands it the events each committed
//! transaction appended, after the commit and outside any transaction, so
//! an observer (the binary's plugin hooks) can take as long as it likes
//! without holding the board.

use crate::board::Board;
use crate::model::Event;

/// Sees the events of each committed board transaction.
pub trait Observer: Send + Sync {
    /// Called once per commit with the events it appended, oldest first.
    fn observe(&self, board: &Board, events: &[Event]);
}
