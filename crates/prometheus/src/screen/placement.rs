//! Where the bot screen runs (spec: execution-environments, Bot screen:
//! `bot_desktop.placement`).

use pmt_config::schema::{BotDesktopConfig, ScreenPlacement};
use pmt_core::error::{Error, codes};

/// Where the screen of a profile goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Where {
    /// On the gateway host (the `local` backend, or `placement =
    /// "gateway"`).
    Host,
    /// Inside the terminal backend: a container or an SSH host.
    Backend(Hosting),
}

/// A terminal backend that can host the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosting {
    /// The container backend (Docker or Podman): the screen runs in the
    /// profile's desktop container.
    Container,
    /// The SSH backend: the screen runs on the SSH host.
    Ssh,
}

/// The backend names that host the screen inside themselves.
fn hosting(backend: &str) -> Option<Hosting> {
    match backend {
        "container" | "docker" | "podman" => Some(Hosting::Container),
        "ssh" => Some(Hosting::Ssh),
        _ => None,
    }
}

/// Where the screen of a profile with terminal backend `backend` runs.
///
/// # Errors
///
/// `screen.unavailable` for a backend that cannot host a display, unless
/// `placement = "gateway"` opts in to the host.
pub fn resolve(config: &BotDesktopConfig, backend: &str) -> Result<Where, Error> {
    match config.placement {
        ScreenPlacement::Gateway => return Ok(Where::Host),
        ScreenPlacement::Auto | ScreenPlacement::Terminal if backend == "local" => {
            return Ok(Where::Host);
        }
        ScreenPlacement::Auto | ScreenPlacement::Terminal => {}
    }
    hosting(backend).map(Where::Backend).ok_or_else(|| {
        Error::new(
            &codes::SCREEN_UNAVAILABLE,
            format!("the '{backend}' terminal backend cannot host the bot screen"),
        )
        .with_hint(
            "set bot_desktop.placement = \"gateway\" to run it on the gateway host (outside the terminal sandbox), or use the local, container or ssh backend",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(placement: ScreenPlacement) -> BotDesktopConfig {
        BotDesktopConfig {
            placement,
            ..BotDesktopConfig::default()
        }
    }

    /// Scenario: Sandbox that cannot host a display.
    #[test]
    fn a_backend_without_a_display_is_refused_and_points_to_gateway() {
        let refused = resolve(&config(ScreenPlacement::Auto), "modal").unwrap_err();
        assert_eq!(refused.code(), &codes::SCREEN_UNAVAILABLE);
        assert!(
            refused
                .hint()
                .unwrap_or_default()
                .contains("bot_desktop.placement = \"gateway\""),
            "{refused:?}"
        );
        assert_eq!(
            resolve(&config(ScreenPlacement::Gateway), "modal").unwrap(),
            Where::Host
        );
    }

    #[test]
    fn auto_places_the_screen_where_the_terminal_runs() {
        let auto = config(ScreenPlacement::Auto);
        assert_eq!(resolve(&auto, "local").unwrap(), Where::Host);
        assert_eq!(
            resolve(&auto, "container").unwrap(),
            Where::Backend(Hosting::Container)
        );
        assert_eq!(resolve(&auto, "ssh").unwrap(), Where::Backend(Hosting::Ssh));
        assert_eq!(
            resolve(&config(ScreenPlacement::Terminal), "singularity")
                .unwrap_err()
                .code(),
            &codes::SCREEN_UNAVAILABLE
        );
    }
}
