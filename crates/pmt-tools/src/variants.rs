//! Model-dependent schema variants (spec: tool-system, Model-dependent
//! schema variants).
//!
//! A tool may advertise a different description and argument schema
//! depending on the session's main route: its wire format, its model
//! family and the auxiliary capabilities it can reach. The variant is
//! picked once, when the session's tool surface is built, and frozen
//! with it: a model switch mid-session keeps it until the next session,
//! a compaction rebuild or a `--now` rebuild. Arguments are always
//! validated against the tool's full schema (the union of its variants),
//! so a call shaped for another variant (a replayed or resumed one) is
//! still accepted.

use std::collections::BTreeSet;
use std::sync::Arc;

use pmt_config::schema::WireFormat;
use serde_json::Value;

/// The auxiliary capability of hosted OCR (scanned PDF pages).
pub const OCR: &str = "ocr";

/// The family a model belongs to, for the tool formats it was trained on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelFamily {
    /// OpenAI GPT, o-series and Codex models.
    OpenAi,
    /// Anthropic Claude models.
    Anthropic,
    /// Google Gemini and Gemma models.
    Google,
    /// Anything else.
    #[default]
    Other,
}

impl ModelFamily {
    /// The family of `model` served by `provider`: the model id decides
    /// (an aggregator's `anthropic/claude-...` is Anthropic), then the
    /// provider.
    #[must_use]
    pub fn of(provider: &str, model: &str) -> Self {
        let base = model
            .rsplit('/')
            .next()
            .unwrap_or(model)
            .to_ascii_lowercase();
        let o_series = base.len() >= 2
            && base.starts_with('o')
            && base[1..2].chars().all(|c| c.is_ascii_digit());
        if base.starts_with("gpt")
            || base.starts_with("chatgpt")
            || base.starts_with("codex")
            || o_series
        {
            return Self::OpenAi;
        }
        if base.starts_with("claude") {
            return Self::Anthropic;
        }
        if base.starts_with("gemini") || base.starts_with("gemma") {
            return Self::Google;
        }
        let provider = provider.to_ascii_lowercase();
        if provider.starts_with("openai") {
            Self::OpenAi
        } else if provider.starts_with("anthropic") {
            Self::Anthropic
        } else if provider.starts_with("google") || provider.starts_with("gemini") {
            Self::Google
        } else {
            Self::Other
        }
    }
}

/// The main-route properties a variant is chosen by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteTraits {
    /// The route's wire format (`None`: unknown).
    pub wire_format: Option<WireFormat>,
    /// The model's family.
    pub family: ModelFamily,
    /// Auxiliary capabilities the session can reach ([`OCR`]).
    pub auxiliary: BTreeSet<String>,
    /// The model's context window in tokens, when known (tool search
    /// sizes its threshold by it).
    pub context_window: Option<u64>,
}

impl RouteTraits {
    /// The traits of `model` on `provider` over `wire_format`.
    #[must_use]
    pub fn of(provider: &str, model: &str, wire_format: WireFormat) -> Self {
        Self {
            wire_format: Some(wire_format),
            family: ModelFamily::of(provider, model),
            auxiliary: BTreeSet::new(),
            context_window: None,
        }
    }

    /// The same traits for a model whose window is `tokens`.
    #[must_use]
    pub fn with_context_window(mut self, tokens: Option<u64>) -> Self {
        self.context_window = tokens;
        self
    }
}

/// What one variant advertises; `None` keeps the descriptor's own.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaVariant {
    /// The variant's name (recorded with the frozen surface).
    pub name: &'static str,
    /// The description it advertises.
    pub description: Option<String>,
    /// The argument schema it advertises.
    pub parameters: Option<Value>,
}

impl SchemaVariant {
    /// The variant advertising the descriptor as it is.
    #[must_use]
    pub fn base(name: &'static str) -> Self {
        Self {
            name,
            description: None,
            parameters: None,
        }
    }
}

/// Picks a tool's variant for a route.
pub type VariantPicker = Arc<dyn Fn(&RouteTraits) -> SchemaVariant + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::ModelFamily;

    #[test]
    fn the_model_id_decides_the_family_before_the_provider() {
        assert_eq!(ModelFamily::of("openai", "gpt-5"), ModelFamily::OpenAi);
        assert_eq!(
            ModelFamily::of("openrouter", "openai/o3-mini"),
            ModelFamily::OpenAi
        );
        assert_eq!(ModelFamily::of("mock", "codex-mini"), ModelFamily::OpenAi);
        assert_eq!(
            ModelFamily::of("openrouter", "anthropic/claude-sonnet-4"),
            ModelFamily::Anthropic
        );
        assert_eq!(
            ModelFamily::of("openai", "claude-x"),
            ModelFamily::Anthropic
        );
        assert_eq!(
            ModelFamily::of("google", "gemini-2.5-pro"),
            ModelFamily::Google
        );
        assert_eq!(
            ModelFamily::of("anthropic", "some-model"),
            ModelFamily::Anthropic
        );
        assert_eq!(ModelFamily::of("mock", "omni"), ModelFamily::Other);
        assert_eq!(ModelFamily::of("ollama", "qwen3"), ModelFamily::Other);
    }
}
