//! `/context [all]` (alias `/ctx`): the context window view, computed
//! locally with no model call and no change to the prompt cache (spec:
//! context-compression, Context window view). Messaging platforms get a
//! text gauge instead of the grid.

use pmt_agent::turn::window::SkillCost;
use pmt_agent::turn::window_render::{Style, render};
use pmt_core::error::{Error, codes};
use pmt_skills::session::SkillsSession;

use super::Ctx;
use crate::slash::Surface;

/// `/context [all]`.
pub(super) async fn context(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let all = match args.trim() {
        "" => false,
        "all" => true,
        _ => return Err(Error::new(&codes::CLI_USAGE, "usage: /context [all]")),
    };
    let mut view = ctx.live.agent.context_view().await?;
    if all {
        view.skills = skill_costs(&ctx.live.skills);
    }
    let style = match ctx.surface {
        Surface::Gateway => Style::Text,
        _ => Style::Grid,
    };
    Ok(render(&view, style, all))
}

/// What each indexed skill costs in the index and when loaded whole.
fn skill_costs(skills: &SkillsSession) -> Vec<SkillCost> {
    let (catalog, config) = skills.catalog();
    let tokens = |text: &str| (text.len() as u64).div_ceil(4);
    skills
        .indexed(&catalog, &config)
        .into_iter()
        .map(|skill| {
            let index_tokens = tokens(&format!("- {}: {}", skill.name, skill.description));
            SkillCost {
                name: skill.name.clone(),
                index_tokens,
                full_tokens: index_tokens + tokens(&skill.body),
            }
        })
        .collect()
}
