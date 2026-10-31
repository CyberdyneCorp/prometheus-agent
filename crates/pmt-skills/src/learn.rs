//! `/learn <sources>` (spec: skills, /learn distils a skill from sources).
//!
//! `/learn` adds no tool: it runs a normal agent turn whose user message
//! carries a fixed authoring instruction. The agent gathers the sources
//! with its own tools and writes the skill with `skill_manage`, so the
//! write goes through the usual validation, scan and write approval.

use pmt_core::error::{Error, codes};

/// Gathered material beyond this many characters becomes a knowledge-base
/// skill.
pub const KNOWLEDGE_BASE_CHARS: usize = 50_000;

/// `/learn` usage.
pub const USAGE: &str = "usage: /learn <directories, files, URLs, notes or \"this conversation\">";

/// The user message of `/learn <sources>`; `description_chars` is
/// `skills.index_description_chars`.
///
/// # Errors
///
/// `cli.usage` when no source is named.
pub fn message(sources: &str, description_chars: u32) -> Result<String, Error> {
    let sources = sources.trim();
    if sources.is_empty() {
        return Err(Error::new(&codes::CLI_USAGE, USAGE));
    }
    Ok(format!(
        "[The user ran /learn. Distil a reusable skill from the sources below.]\n\n\
Sources: {sources}\n\n\
1. Gather every source with your own tools: read directories and files, fetch URLs with the web tools, \
use the current conversation or the pasted notes when they are named.\n\
2. Call skills_list first. If a skill already covers this topic, read it with skill_view and patch it \
with skill_manage instead of creating a duplicate.\n\
3. Otherwise create one skill with skill_manage. Its description is one sentence of at most \
{description_chars} characters ending with a period, without marketing words. The body uses the \
sections, in this order: ## When to Use, ## Procedure, ## Pitfalls, ## Verification.\n\
4. Write only commands, flags and APIs the sources show; invent nothing. Distil in your own words: \
never copy source passages verbatim.\n\
5. If the gathered material exceeds {KNOWLEDGE_BASE_CHARS} characters (a book, a large manual), \
make a knowledge-base skill: a lean SKILL.md with the core concepts and an index of the reference \
files, plus one distilled file per chapter or topic under references/ written with \
skill_manage write_file.\n\
6. Finish with the skill's name and what it covers."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sources_are_a_usage_error() {
        assert_eq!(message("  ", 60).unwrap_err().exit_code(), 2);
    }
}
