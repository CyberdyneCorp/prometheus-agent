//! The `vision` toolset (`vision_analyze`) and the image input routing the
//! turn loop shares (spec: web-and-media-tools, `vision_analyze` tool; Image
//! input routing).
//!
//! [`image`] reads and transforms image bytes, [`routing`] decides how an
//! image reaches the model, and [`tool`] is `vision_analyze`. The session's
//! [`VisionSession`] knows its main route (vision support, whether tool
//! results carry images) and its `vision` auxiliary route, which the turn
//! loop provides as a [`Describer`].

pub mod image;
pub mod routing;
pub mod tool;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_net::fetch::Fetcher;

pub use image::ImageData;
pub use routing::{Delivery, Inline, RoutingInputs};
pub use tool::tools;

/// What a description returns.
pub type DescribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// The `vision` auxiliary route: describes an image to answer a question.
pub trait Describer: Send + Sync {
    /// The route's answer to `question` about `image`.
    fn describe<'a>(&'a self, image: &'a ImageData, question: &'a str) -> DescribeFuture<'a>;
}

/// What a [`VisionSession`] is built from.
pub struct VisionSetup {
    /// The routing inputs of the session's main route.
    pub routing: RoutingInputs,
    /// The main route's wire format carries images inside tool results.
    pub tool_result_images: bool,
    /// The `vision` auxiliary route; `None` when none resolves.
    pub describer: Option<Arc<dyn Describer>>,
    /// The session's URL policy for downloads.
    pub fetcher: Arc<Fetcher>,
    /// Where images attached to tool results are written.
    pub media_dir: PathBuf,
}

/// One session's vision state.
pub struct VisionSession {
    setup: VisionSetup,
    attached: Mutex<Vec<String>>,
}

/// The reason `vision_analyze` gives without any route.
pub const NO_ROUTE: &str = "no vision-capable route";

impl VisionSession {
    /// A session over `setup`.
    #[must_use]
    pub fn new(setup: VisionSetup) -> Self {
        Self {
            setup,
            attached: Mutex::default(),
        }
    }

    /// The images of the current user message (by the reference the model
    /// sees); the turn loop sets them when a user message is sent.
    pub fn set_current_attachments(&self, references: Vec<String>) {
        *self.attached.lock().unwrap_or_else(PoisonError::into_inner) = references;
    }

    fn is_attached(&self, reference: &str) -> bool {
        self.attached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|r| r == reference)
    }

    /// Whether `vision_analyze` returns the image itself (pixels in the
    /// tool result) rather than a description.
    #[must_use]
    pub fn native(&self) -> bool {
        self.setup.tool_result_images && self.setup.routing.delivery() == Delivery::Native
    }

    /// Why `vision_analyze` cannot work in this session, if it cannot (the
    /// session leaves it out of its surface then).
    #[must_use]
    pub fn unavailable(&self) -> Option<&'static str> {
        (!self.native() && self.setup.describer.is_none()).then_some(NO_ROUTE)
    }
}
