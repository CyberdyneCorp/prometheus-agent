//! `vision_analyze` of a session (spec: web-and-media-tools,
//! `vision_analyze` tool; Image embed budget, per-image cap and replay).
//!
//! The routing decision, the `vision` auxiliary route and the budgets are
//! settled when the session opens ([`Factory::vision_plan`]); the
//! environment then builds the [`VisionSession`] with the session's own URL
//! policy. A session moved to another worktree keeps its `VisionSession`,
//! so its per-image counts survive the move.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_core::id::SessionId;
use pmt_net::fetch::Fetcher;
use pmt_providers::resolve::{Target, Trace};
use pmt_tools::vision::budget::{clamp_target, per_image_cap};
use pmt_tools::vision::routing::{RoutingInputs, aux_explicit, tool_result_images};
use pmt_tools::vision::{Describer, VisionSession, VisionSetup};

use super::Factory;

/// What a session's `vision_analyze` is built from.
pub enum Vision {
    /// No `vision_analyze` (an `mcp serve` session).
    Off,
    /// A new session's, built with its environment.
    Planned(Plan),
    /// A running session's, kept across a move (with its video route).
    Kept(
        Arc<VisionSession>,
        Option<Arc<dyn pmt_media::video::VideoDescriber>>,
    ),
}

/// Everything of a [`VisionSetup`] but the session's URL policy.
#[derive(Clone)]
pub struct Plan {
    routing: RoutingInputs,
    tool_result_images: bool,
    describer: Option<Arc<dyn Describer>>,
    embed_target: u64,
    per_image_cap: Option<u32>,
    /// The `vision` auxiliary route when its model takes video.
    video: Option<Arc<dyn pmt_media::video::VideoDescriber>>,
}

impl Vision {
    /// The video-capable `vision` route, when the session has one.
    #[must_use]
    pub fn video(&self) -> Option<Arc<dyn pmt_media::video::VideoDescriber>> {
        match self {
            Self::Off => None,
            Self::Planned(plan) => plan.video.clone(),
            Self::Kept(_, video) => video.clone(),
        }
    }

    /// The session's `VisionSession`; `fetcher` gives its URL policy and
    /// `media_dir` is where returned images are kept.
    pub fn session(
        &self,
        fetcher: impl FnOnce() -> Option<Arc<Fetcher>>,
        media_dir: PathBuf,
    ) -> Option<Arc<VisionSession>> {
        match self {
            Self::Off => None,
            Self::Kept(session, _) => Some(Arc::clone(session)),
            Self::Planned(plan) => {
                let plan = plan.clone();
                Some(Arc::new(VisionSession::new(VisionSetup {
                    routing: plan.routing,
                    tool_result_images: plan.tool_result_images,
                    describer: plan.describer,
                    fetcher: fetcher()?,
                    media_dir,
                    embed_target: plan.embed_target,
                    per_image_cap: plan.per_image_cap,
                })))
            }
        }
    }
}

impl Factory {
    /// The `vision_analyze` plan of session `id` on `target`; `delegated`
    /// for a delegated child (whose unset per-image cap is 3). The
    /// auxiliary route is resolved only when the main route cannot take
    /// the image itself.
    pub(super) async fn vision_plan(
        &self,
        id: SessionId,
        target: &Target,
        delegated: bool,
    ) -> Vision {
        let route = match self
            .models
            .resolver()
            .resolve_target(target, &mut Trace::default())
            .await
        {
            Ok(route) => route,
            Err(err) => {
                tracing::warn!(error = %err, "vision_analyze unavailable this session");
                return Vision::Off;
            }
        };
        let config = self.scope.config();
        let (meta, _) = self
            .models
            .catalog()
            .metadata(&route.provider, &route.model);
        let routing = RoutingInputs {
            mode: config.vision.image_input_mode,
            main_vision: meta.supports_vision,
            aux_explicit: aux_explicit(config),
        };
        let tool_result_images = tool_result_images(route.wire_format);
        let native =
            tool_result_images && routing.delivery() == pmt_tools::vision::Delivery::Native;
        let describer = if native {
            None
        } else {
            self.models
                .describer(&route, id)
                .await
                .inspect_err(|err| tracing::warn!(error = %err, "no vision route this session"))
                .ok()
                .flatten()
                .map(|d| Arc::new(d) as Arc<dyn Describer>)
        };
        // Boxed: the route resolution would otherwise grow every session
        // opening future.
        let video = Box::pin(self.models.video_route(&route, id))
            .await
            .map(|aux| Arc::new(aux) as Arc<dyn pmt_media::video::VideoDescriber>);
        Vision::Planned(Plan {
            video,
            routing,
            tool_result_images,
            describer,
            embed_target: clamp_target(config.vision.embed_target_bytes),
            per_image_cap: per_image_cap(config.vision.max_calls_per_image, delegated),
        })
    }
}
