//! Replaying a recorded system prompt (spec: replay-and-evaluation,
//! Divergence detection; Session recordings).
//!
//! A recording keeps each prompt build layer by layer. A replay rebuilds
//! the prompt with the current build, then puts back the recorded text of
//! every layer that comes from the recorded machine and moment rather than
//! from the harness (identity, platform hint, skills, personality, caller
//! system message, project context, memory, plugin sections, session line
//! and runtime block): what is left to differ is the harness's own text
//! (tool and model guidance), the layer order and the tool schemas, which
//! is what a replay must catch.

use pmt_core::id::SessionId;

use super::{Layer, Section, SystemPrompt};

/// The layers a replay rebuilds; every other one is pinned.
pub const REBUILT: [Layer; 2] = [Layer::ToolGuidance, Layer::ModelGuidance];

/// Supplies the recorded prompt of each build of a replayed session.
pub trait PromptPins: Send + Sync {
    /// The prompt `session` uses in place of the freshly `built` one.
    fn pin(&self, session: SessionId, built: SystemPrompt) -> SystemPrompt;
}

impl Layer {
    /// The layer named `name` ([`Layer::as_str`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.as_str() == name)
    }
}

/// `built` with every pinned layer's text taken from `recorded`
/// (`(layer, text)` pairs; a pinned layer missing there is left out).
#[must_use]
pub fn pin(built: &SystemPrompt, recorded: &[(String, String)]) -> SystemPrompt {
    let mut sections: Vec<Section> = built
        .sections()
        .iter()
        .filter(|s| REBUILT.contains(&s.layer))
        .cloned()
        .collect();
    sections.extend(recorded.iter().filter_map(|(name, text)| {
        let layer = Layer::parse(name).filter(|l| !REBUILT.contains(l))?;
        Some(Section {
            layer,
            text: text.clone(),
        })
    }));
    SystemPrompt::new(sections)
}

/// `prompt` as a recording keeps it.
#[must_use]
pub fn sections(prompt: &SystemPrompt) -> Vec<(String, String)> {
    prompt
        .sections()
        .iter()
        .map(|s| (s.layer.as_str().to_owned(), s.text.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(layer: Layer, text: &str) -> Section {
        Section {
            layer,
            text: text.to_owned(),
        }
    }

    #[test]
    fn harness_layers_are_rebuilt_and_the_rest_comes_from_the_recording() {
        let built = SystemPrompt::new(vec![
            section(Layer::Identity, "new identity"),
            section(Layer::ToolGuidance, "new guidance"),
            section(Layer::Runtime, "new runtime"),
        ]);
        let recorded = vec![
            ("identity".to_owned(), "old identity".to_owned()),
            ("tool_guidance".to_owned(), "old guidance".to_owned()),
            ("memory".to_owned(), "old memory".to_owned()),
            ("runtime".to_owned(), "old runtime".to_owned()),
        ];
        let pinned = pin(&built, &recorded);
        assert_eq!(
            sections(&pinned),
            [
                ("identity".to_owned(), "old identity".to_owned()),
                ("tool_guidance".to_owned(), "new guidance".to_owned()),
                ("memory".to_owned(), "old memory".to_owned()),
                ("runtime".to_owned(), "old runtime".to_owned()),
            ]
        );
    }
}
