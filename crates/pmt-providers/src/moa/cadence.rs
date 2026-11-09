//! Advisor cadence (spec: mixture-of-agents, Advisor cadence): on which
//! model iterations of a user turn the references run. In between, the
//! most recent guidance of the turn is reused.

use pmt_config::schema::MoaFanout;

/// A preset's normalised `fanout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fanout {
    /// The first iteration of each user turn (the default).
    UserTurn,
    /// Every iteration.
    PerIteration,
    /// Iterations 1 and every multiple of N (N >= 2).
    EveryN(u32),
}

impl Fanout {
    /// The cadence `value` names, or why it is malformed.
    ///
    /// # Errors
    ///
    /// The malformed value, as the WARN shows it.
    pub fn parse(value: &MoaFanout) -> Result<Self, String> {
        match value {
            MoaFanout::Text(text) => match text.trim() {
                "user_turn" => Ok(Self::UserTurn),
                "per_iteration" => Ok(Self::PerIteration),
                other => other
                    .strip_prefix("every_n:")
                    .and_then(|n| n.trim().parse::<i64>().ok())
                    .and_then(every_n)
                    .ok_or_else(|| format!("'{other}'")),
            },
            MoaFanout::Table { mode, n } if mode == "every_n" => n
                .and_then(every_n)
                .ok_or_else(|| format!("{{ mode = \"every_n\", n = {n:?} }}")),
            MoaFanout::Table { mode, .. } => Err(format!("{{ mode = \"{mode}\" }}")),
        }
    }

    /// Whether the references run on iteration `i` (1-based) of a turn.
    #[must_use]
    pub fn runs(self, i: u32) -> bool {
        match self {
            Self::UserTurn => i <= 1,
            Self::PerIteration => true,
            Self::EveryN(n) => i <= 1 || i.is_multiple_of(n),
        }
    }
}

fn every_n(n: i64) -> Option<Fanout> {
    u32::try_from(n)
        .ok()
        .filter(|n| *n >= 2)
        .map(Fanout::EveryN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> MoaFanout {
        MoaFanout::Text(value.to_owned())
    }

    #[test]
    fn every_third_iteration_runs_on_one_three_and_six() {
        let fanout = Fanout::parse(&text("every_n:3")).unwrap();
        let ran: Vec<u32> = (1..=7).filter(|i| fanout.runs(*i)).collect();
        assert_eq!(ran, [1, 3, 6]);
        let table = MoaFanout::Table {
            mode: "every_n".into(),
            n: Some(3),
        };
        assert_eq!(Fanout::parse(&table), Ok(Fanout::EveryN(3)));
    }

    #[test]
    fn the_modes_and_malformed_values() {
        assert_eq!(Fanout::parse(&text("user_turn")), Ok(Fanout::UserTurn));
        assert!(Fanout::UserTurn.runs(1) && !Fanout::UserTurn.runs(2));
        assert!(Fanout::parse(&text("per_iteration")).unwrap().runs(5));
        for bad in ["sometimes", "every_n:1", "every_n:x", "every_n:-2"] {
            assert!(Fanout::parse(&text(bad)).is_err(), "{bad}");
        }
        let missing = MoaFanout::Table {
            mode: "every_n".into(),
            n: None,
        };
        assert!(Fanout::parse(&missing).is_err());
    }
}
