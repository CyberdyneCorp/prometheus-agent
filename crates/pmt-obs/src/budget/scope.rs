//! Budget scopes, their limits from `[budgets]` and the ledger rows each
//! one counts (spec: observability, Hard budgets).

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};
use pmt_config::schema::BudgetsConfig;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_store::{Store, UsageFilter};

/// A budget scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// The current turn (with its compression, auxiliary and delegated
    /// children's calls).
    Turn,
    /// The session and its descendants.
    Session,
    /// The whole root lineage.
    Root,
    /// The profile's calendar day.
    Daily,
    /// The profile's calendar month.
    Monthly,
    /// The root lineage of an unattended run (cron, unattended gateway
    /// sessions, background delegations, harness evolution).
    Unattended,
}

impl Scope {
    /// Every scope, in the order checks run.
    pub const ALL: [Self; 6] = [
        Self::Turn,
        Self::Session,
        Self::Root,
        Self::Daily,
        Self::Monthly,
        Self::Unattended,
    ];

    /// The scope's name in events, errors and `/budget set`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Session => "session",
            Self::Root => "root",
            Self::Daily => "daily",
            Self::Monthly => "monthly",
            Self::Unattended => "unattended",
        }
    }

    /// The `[budgets]` key of the scope's USD limit.
    #[must_use]
    pub const fn usd_key(self) -> &'static str {
        match self {
            Self::Turn => "turn_usd",
            Self::Session => "session_usd",
            Self::Root => "root_usd",
            Self::Daily => "daily_usd",
            Self::Monthly => "monthly_usd",
            Self::Unattended => "unattended_root_usd",
        }
    }

    /// The scope named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == name)
    }
}

/// Sets the USD limit of `scope` in `config` (what `/budget set` wrote to
/// the file, applied to the running session at once).
pub fn set_usd(config: &mut BudgetsConfig, scope: Scope, usd: f64) {
    let field = match scope {
        Scope::Turn => &mut config.turn_usd,
        Scope::Session => &mut config.session_usd,
        Scope::Root => &mut config.root_usd,
        Scope::Daily => &mut config.daily_usd,
        Scope::Monthly => &mut config.monthly_usd,
        Scope::Unattended => &mut config.unattended_root_usd,
    };
    *field = usd;
}

/// What a limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unit {
    /// US dollars.
    Usd,
    /// Input, output and cache tokens.
    Tokens,
}

impl Unit {
    /// `usd` or `tokens`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usd => "usd",
            Self::Tokens => "tokens",
        }
    }
}

/// One configured (non-zero) limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limit {
    /// The scope.
    pub scope: Scope,
    /// The unit.
    pub unit: Unit,
    /// The limit.
    pub limit: f64,
}

#[allow(clippy::cast_precision_loss)] // token limits stay far below 2^52
fn tokens(limit: u64) -> f64 {
    limit as f64
}

/// The limits that apply to a call: every non-zero `[budgets]` limit, the
/// unattended one only for unattended runs, the turn ones only inside a
/// turn.
#[must_use]
pub fn limits(config: &BudgetsConfig, in_turn: bool, unattended: bool) -> Vec<Limit> {
    let candidates = [
        (Scope::Turn, Unit::Tokens, tokens(config.turn_tokens)),
        (Scope::Turn, Unit::Usd, config.turn_usd),
        (Scope::Session, Unit::Tokens, tokens(config.session_tokens)),
        (Scope::Session, Unit::Usd, config.session_usd),
        (Scope::Root, Unit::Tokens, tokens(config.root_tokens)),
        (Scope::Root, Unit::Usd, config.root_usd),
        (Scope::Daily, Unit::Usd, config.daily_usd),
        (Scope::Monthly, Unit::Usd, config.monthly_usd),
        (Scope::Unattended, Unit::Usd, config.unattended_root_usd),
    ];
    candidates
        .into_iter()
        .filter(|&(scope, _, limit)| {
            limit > 0.0
                && (scope != Scope::Turn || in_turn)
                && (scope != Scope::Unattended || unattended)
        })
        .map(|(scope, unit, limit)| Limit { scope, unit, limit })
        .collect()
}

/// Where a call happens: its session, the root of its lineage, the
/// session's descendants and the turn start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The calling session.
    pub session: SessionId,
    /// Its root session.
    pub root: SessionId,
    /// The session and every descendant (continuations, forks, children).
    pub subtree: Vec<SessionId>,
    /// When the current turn started (`None` outside a turn).
    pub turn_started_at: Option<Timestamp>,
}

impl Place {
    /// Looks the session's lineage up in the store.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn of(
        store: &Store,
        session: SessionId,
        turn_started_at: Option<Timestamp>,
    ) -> Result<Self, Error> {
        let root = store.session(session)?.root_session_id;
        let mut subtree = vec![session];
        subtree.extend(store.tree(session)?.into_iter().map(|node| node.id));
        Ok(Self {
            session,
            root,
            subtree,
            turn_started_at,
        })
    }
}

/// The ledger rows a scope counts, and the period key a warning is given
/// once per.
pub(crate) fn window(
    scope: Scope,
    place: &Place,
    now: Timestamp,
    zone: &TimeZone,
) -> Result<(UsageFilter, String), Error> {
    let session_rows = || UsageFilter {
        sessions: Some(place.subtree.clone()),
        ..UsageFilter::default()
    };
    Ok(match scope {
        Scope::Turn => {
            let started = place.turn_started_at.unwrap_or(now);
            let filter = UsageFilter {
                since: Some(started),
                ..session_rows()
            };
            (
                filter,
                format!("{}@{}", place.session, started.as_millisecond()),
            )
        }
        Scope::Session => (session_rows(), place.session.to_string()),
        Scope::Root | Scope::Unattended => (
            UsageFilter {
                root: Some(place.root),
                ..UsageFilter::default()
            },
            place.root.to_string(),
        ),
        Scope::Daily => {
            let today = now.to_zoned(zone.clone()).date();
            (
                calendar(today, today.tomorrow().map_err(internal)?, zone)?,
                today.to_string(),
            )
        }
        Scope::Monthly => {
            let first = now.to_zoned(zone.clone()).date().first_of_month();
            let next = first.checked_add(1.month()).map_err(internal)?;
            (
                calendar(first, next, zone)?,
                format!("{}-{:02}", first.year(), first.month()),
            )
        }
    })
}

/// Rows from the start of `from` to the start of `to` in `zone`.
fn calendar(from: Date, to: Date, zone: &TimeZone) -> Result<UsageFilter, Error> {
    let start = |date: Date| -> Result<Timestamp, Error> {
        Ok(date.to_zoned(zone.clone()).map_err(internal)?.timestamp())
    };
    Ok(UsageFilter {
        since: Some(start(from)?),
        until: Some(start(to)?),
        ..UsageFilter::default()
    })
}

#[allow(clippy::needless_pass_by_value)] // used as a `map_err` function
fn internal(err: jiff::Error) -> Error {
    Error::new(
        &pmt_core::error::codes::CONFIG_TIMEZONE,
        format!("cannot compute a budget period: {err}"),
    )
}
