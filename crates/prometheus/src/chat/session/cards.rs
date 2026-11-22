//! The cards a session's surface draws: connection cards for
//! `manage_connections` and approval cards with a row per item for
//! `catalog_manage`.

/// The cards a session's surface draws.
#[derive(Debug, Clone, Copy)]
pub(super) struct Cards {
    /// Connection cards (`manage_connections`).
    pub(super) connections: bool,
    /// Approval cards with a row per item (`catalog_manage`).
    pub(super) catalog: bool,
}

impl Cards {
    pub(super) fn of(interactive: bool, surface: &pmt_tools::SessionSurface) -> Self {
        Self {
            connections: shows_cards(interactive, surface),
            catalog: draws_approval_cards(interactive, surface),
        }
    }
}

/// Whether a session draws approval cards with a row per item: the TUI
/// and the desktop (spec: plugin-system, Catalog tool for agent-initiated
/// installs).
fn draws_approval_cards(interactive: bool, surface: &pmt_tools::SessionSurface) -> bool {
    use pmt_tools::SessionSurface::{Desktop, Onboarding, Tui};
    interactive && matches!(surface, Tui | Desktop | Onboarding)
}

/// Whether a session shows connection cards: an interactive terminal,
/// the TUI or the desktop (messaging chats, one-shot runs and cron get
/// links instead).
fn shows_cards(interactive: bool, surface: &pmt_tools::SessionSurface) -> bool {
    use pmt_tools::SessionSurface::{Cli, Desktop, Tui};
    interactive && matches!(surface, Cli | Tui | Desktop)
}
