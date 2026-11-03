//! What background tasks (`/background`, spec: messaging-gateway,
//! Background sessions from chat) tell the user: one wording for chats,
//! the line interface and the TUI.

/// Characters of the prompt the start notice repeats.
const EXCERPT: usize = 60;

/// What starting a task answers (every surface says the same).
#[must_use]
pub fn started(prompt: &str, id: &str) -> String {
    format!("Background task started: {} (id {id})", excerpt(prompt))
}

/// What a task that answered `answer` reports.
#[must_use]
pub fn complete(answer: &str) -> String {
    format!("✅ Background task complete\n\n{answer}")
        .trim_end()
        .to_owned()
}

/// What a task that failed for `reason` reports.
#[must_use]
pub fn failed(reason: &str) -> String {
    format!("❌ Background task failed: {reason}")
}

/// What refusing a task over the per-chat cap answers.
#[must_use]
pub fn too_many(max: u32) -> String {
    format!("Too many background tasks running ({max})")
}

/// What the start notice shows of `prompt`.
fn excerpt(prompt: &str) -> String {
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT {
        return flat;
    }
    let cut: String = flat.chars().take(EXCERPT).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_prompt_is_cut_with_an_ellipsis() {
        let long = "word ".repeat(40);
        let cut = excerpt(&long);
        assert!(cut.ends_with('…') && cut.chars().count() <= EXCERPT + 1);
        assert_eq!(excerpt("  short\nprompt "), "short prompt");
        assert_eq!(too_many(3), "Too many background tasks running (3)");
    }
}
