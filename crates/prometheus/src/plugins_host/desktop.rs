//! The desktop app's plugin panels (spec: desktop-app, Plugin UI
//! contributions are isolated): `prometheus --json plugins panels`, which
//! the desktop shell runs through its bundled binary to learn the `[ui]`
//! panels of the profile's plugins (`pmt_plugins::desktop_ui`).

use pmt_core::error::Error;
use serde_json::json;

use crate::context::Context;
use crate::output::json as print_json;

/// `plugins panels [--all-profiles]`: the panels as JSON, each with the
/// profile it belongs to.
///
/// # Errors
///
/// Home and profile errors.
pub fn panels(ctx: &Context<'_>, all_profiles: bool) -> Result<u8, Error> {
    let scopes = if all_profiles {
        let home = ctx.home()?;
        home.profiles()?
            .into_iter()
            .filter_map(|name| {
                let dir = home.existing_profile(&name).ok()?;
                pmt_config::ProfileScope::open(
                    dir,
                    &pmt_config::Overrides::default(),
                    pmt_config::Launch::Other,
                )
                .ok()
            })
            .collect()
    } else {
        vec![ctx.scope()?]
    };
    let mut panels = Vec::new();
    let mut skipped = Vec::new();
    for scope in &scopes {
        let version = super::running_version();
        let blocked = super::catalog::kill_list(scope);
        let inputs = super::inputs(scope, scope.dir(), &version, &blocked);
        let found = pmt_plugins::desktop_ui::panels(&pmt_plugins::load::survey(&inputs));
        let profile = scope.name().to_string();
        panels.extend(found.panels.into_iter().map(|p| {
            json!({"profile": profile, "id": p.id, "label": p.label, "entry": p.entry, "sdk": p.sdk})
        }));
        skipped.extend(
            found
                .skipped
                .into_iter()
                .map(|s| json!({"profile": profile, "id": s.id, "reason": s.reason})),
        );
    }
    print_json(&json!({"panels": panels, "skipped": skipped}))
}
