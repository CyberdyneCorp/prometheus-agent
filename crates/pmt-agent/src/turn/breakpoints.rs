//! Prompt-cache breakpoints and their lifetime (spec: context-compression,
//! Cache breakpoint plan; Prompt cache TTL).
//!
//! Only a provider with explicit breakpoints (Anthropic Messages) gets
//! markers: at most four per request, all with the session's TTL, at most
//! two at the end of the system-prompt tiers (each tier that ends on a new
//! boundary; the last two are kept, the profile and session tiers of the
//! usual layout) and the rest on the latest cacheable rows. The plan is
//! applied to the per-request copy: markers are never stored and every
//! request is re-planned from scratch. The tool list precedes the system
//! prompt in the cached prefix, so a system breakpoint covers it.
//!
//! `prompt_cache.ttl = "auto"` resolves once per session: five minutes for
//! machine-paced sessions (subagent, cron, webhook, kanban, api, batch),
//! one hour otherwise. A TTL the route's provider cannot keep is lowered to
//! the longest it supports, with one DEBUG log.

use pmt_config::schema::{PromptCacheTtl, WireFormat};
use pmt_providers::request::{CacheTtl, Request};

/// Breakpoints a request carries at most.
pub const MAX_BREAKPOINTS: usize = 4;
/// Breakpoints the system prompt uses at most.
pub const MAX_SYSTEM_BREAKPOINTS: usize = 2;
/// Surfaces whose requests are machine-paced.
const MACHINE_PACED: [&str; 6] = ["subagent", "cron", "webhook", "kanban", "api", "batch"];

/// The TTL `configured` gives a session on `platform`.
#[must_use]
pub fn session_ttl(configured: PromptCacheTtl, platform: &str) -> CacheTtl {
    let resolved = match configured {
        PromptCacheTtl::Auto if MACHINE_PACED.contains(&platform) => PromptCacheTtl::FiveMinutes,
        PromptCacheTtl::Auto => PromptCacheTtl::OneHour,
        other => other,
    };
    match resolved {
        PromptCacheTtl::Off => CacheTtl::Off,
        PromptCacheTtl::FiveMinutes => CacheTtl::FiveMinutes,
        PromptCacheTtl::OneHour | PromptCacheTtl::Auto => CacheTtl::OneHour,
    }
}

/// Whether `wire` takes explicit cache breakpoints.
#[must_use]
pub const fn explicit(wire: WireFormat) -> bool {
    matches!(wire, WireFormat::AnthropicMessages)
}

/// `ttl` lowered to the longest `wire` keeps: only explicit breakpoints
/// carry a one-hour lifetime.
#[must_use]
pub fn supported(ttl: CacheTtl, wire: WireFormat) -> CacheTtl {
    if ttl == CacheTtl::OneHour && !explicit(wire) {
        CacheTtl::FiveMinutes
    } else {
        ttl
    }
}

/// Re-plans every breakpoint of `request` (a per-request copy).
pub fn plan(request: &mut Request, wire: WireFormat) {
    request.cache.tools_breakpoint = false;
    for block in &mut request.system {
        block.cache_breakpoint = false;
    }
    for message in &mut request.messages {
        message.cache_breakpoint = false;
    }
    if !explicit(wire) || request.cache.ttl == CacheTtl::Off {
        return;
    }
    // An empty tier ends where the previous one did: no new boundary.
    let tiers: Vec<usize> = (0..request.system.len())
        .filter(|&i| !request.system[i].text.trim().is_empty())
        .collect();
    let mut used = 0;
    for &i in tiers.iter().rev().take(MAX_SYSTEM_BREAKPOINTS) {
        request.system[i].cache_breakpoint = true;
        used += 1;
    }
    for message in request
        .messages
        .iter_mut()
        .rev()
        .filter(|m| !m.parts.is_empty())
        .take(MAX_BREAKPOINTS - used)
    {
        message.cache_breakpoint = true;
    }
}

/// The breakpoints `request` carries.
#[must_use]
pub fn count(request: &Request) -> usize {
    request.system.iter().filter(|b| b.cache_breakpoint).count()
        + request
            .messages
            .iter()
            .filter(|m| m.cache_breakpoint)
            .count()
        + usize::from(request.cache.tools_breakpoint)
}

#[cfg(test)]
mod tests {
    use pmt_providers::request::{Message, Role, SystemBlock};

    use super::*;

    fn tiered(rows: usize) -> Request {
        let mut request = Request {
            system: ["static", "profile", "session"]
                .iter()
                .map(|t| SystemBlock {
                    text: (*t).to_owned(),
                    cache_breakpoint: true,
                })
                .collect(),
            messages: (0..rows)
                .map(|i| {
                    let role = if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    };
                    Message::text(role, format!("row {i}"))
                })
                .collect(),
            ..Request::default()
        };
        request.cache.tools_breakpoint = true;
        request
    }

    #[test]
    fn a_tiered_prompt_with_long_history_gets_four_breakpoints() {
        let mut request = tiered(20);
        plan(&mut request, WireFormat::AnthropicMessages);
        assert_eq!(count(&request), 4);
        let system: Vec<bool> = request.system.iter().map(|b| b.cache_breakpoint).collect();
        assert_eq!(system, [false, true, true]);
        let marked: Vec<usize> = (0..20)
            .filter(|&i| request.messages[i].cache_breakpoint)
            .collect();
        assert_eq!(marked, [18, 19]);
        // Re-planning never exceeds four.
        plan(&mut request, WireFormat::AnthropicMessages);
        assert_eq!(count(&request), 4);
    }

    #[test]
    fn an_empty_tier_is_no_new_boundary() {
        let mut request = tiered(1);
        request.system[2].text = "  ".into();
        plan(&mut request, WireFormat::AnthropicMessages);
        let system: Vec<bool> = request.system.iter().map(|b| b.cache_breakpoint).collect();
        assert_eq!(system, [true, true, false]);
        assert_eq!(count(&request), 3, "two tiers and the only row");
    }

    #[test]
    fn no_markers_without_explicit_breakpoints_or_with_caching_off() {
        let mut request = tiered(5);
        plan(&mut request, WireFormat::ChatCompletions);
        assert_eq!(count(&request), 0);
        let mut off = tiered(5);
        off.cache.ttl = CacheTtl::Off;
        plan(&mut off, WireFormat::AnthropicMessages);
        assert_eq!(count(&off), 0);
    }

    #[test]
    fn auto_ttl_by_surface() {
        assert_eq!(
            session_ttl(PromptCacheTtl::Auto, "cron"),
            CacheTtl::FiveMinutes
        );
        assert_eq!(
            session_ttl(PromptCacheTtl::Auto, "subagent"),
            CacheTtl::FiveMinutes
        );
        assert_eq!(session_ttl(PromptCacheTtl::Auto, "cli"), CacheTtl::OneHour);
        assert_eq!(
            session_ttl(PromptCacheTtl::Auto, "telegram"),
            CacheTtl::OneHour
        );
        assert_eq!(
            session_ttl(PromptCacheTtl::FiveMinutes, "cli"),
            CacheTtl::FiveMinutes
        );
        assert_eq!(session_ttl(PromptCacheTtl::Off, "cli"), CacheTtl::Off);
        assert_eq!(
            supported(CacheTtl::OneHour, WireFormat::ChatCompletions),
            CacheTtl::FiveMinutes
        );
        assert_eq!(
            supported(CacheTtl::OneHour, WireFormat::AnthropicMessages),
            CacheTtl::OneHour
        );
    }
}
