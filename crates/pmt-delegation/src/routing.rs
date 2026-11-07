//! Child model routing (spec: subagent-delegation, Child model routing):
//! the first of the task's `model`, the agent definition's `model`,
//! `delegation.model` and the parent's current route. A child that pins
//! its own model gets no inherited fallback chain (a failure on its pinned
//! route is that child's failure); an unpinned child runs on the parent's
//! route with the parent's chain.

/// Where a child's model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The task's `model`.
    Task,
    /// The agent definition's `model`.
    Agent,
    /// `delegation.model`.
    Config,
    /// The parent's current route.
    Parent,
}

/// The model a child runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// `provider/model`, or `None` for the parent's route.
    pub model: Option<String>,
    /// Where it came from.
    pub source: Source,
}

impl ModelChoice {
    /// Whether the child pinned a model of its own (no inherited fallback
    /// chain).
    #[must_use]
    pub fn pinned(&self) -> bool {
        self.source != Source::Parent
    }
}

/// Picks a child's model.
#[must_use]
pub fn choose(task: Option<&str>, agent: Option<&str>, config: Option<&str>) -> ModelChoice {
    let named = |m: Option<&str>| {
        m.map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned)
    };
    [
        (named(task), Source::Task),
        (named(agent), Source::Agent),
        (named(config), Source::Config),
    ]
    .into_iter()
    .find_map(|(model, source)| {
        model.map(|m| ModelChoice {
            model: Some(m),
            source,
        })
    })
    .unwrap_or(ModelChoice {
        model: None,
        source: Source::Parent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_named_model_wins_and_only_the_parent_route_is_unpinned() {
        let task = choose(Some("openrouter/cheap-model"), Some("a/b"), Some("c/d"));
        assert_eq!(task.model.as_deref(), Some("openrouter/cheap-model"));
        assert!(task.pinned());
        assert_eq!(choose(None, Some("a/b"), None).source, Source::Agent);
        assert_eq!(choose(Some(" "), None, Some("c/d")).source, Source::Config);
        let inherited = choose(None, None, None);
        assert!(!inherited.pinned() && inherited.model.is_none());
    }
}
