//! Images in model requests (spec: web-and-media-tools, Image input
//! routing; agent-turn-loop, Image input routing).
//!
//! [`AuxDescriber`] is the `vision` auxiliary route as the
//! [`pmt_tools::vision::Describer`] that `vision_analyze` and the routing
//! below use. [`image_part`] turns one image of a message (a user
//! attachment or a tool's image output) into the request part its
//! [`Delivery`] calls for; the turn loop decides the delivery once, when
//! the message is first sent, and keeps it with the message.

use std::sync::Arc;

use pmt_providers::auxiliary::{AuxCaller, AuxRoute};
use pmt_providers::cost::CallContext;
use pmt_providers::request::{Message, Part, Request, Role, SystemBlock};
use pmt_tools::vision::routing::{Inline, inline};
use pmt_tools::vision::{Delivery, DescribeFuture, Describer, ImageData};

/// The system prompt of a `vision` auxiliary call.
const DESCRIBE_SYSTEM: &str = "You are the vision assistant of another model that cannot see \
images. Answer the question about the image precisely and completely: quote visible text \
exactly, give numbers, positions and colours where they matter, and say when something is \
unreadable or absent.";

/// The question asked about an image that replaces pixels in a message.
pub const DESCRIBE_QUESTION: &str = "Describe this image in detail, including any visible text.";

/// The `vision` auxiliary route of a session.
pub struct AuxDescriber {
    caller: Arc<AuxCaller>,
    route: AuxRoute,
    context: CallContext,
}

impl AuxDescriber {
    /// Calls `route` (resolved for `AuxTask::Vision`) through `caller`;
    /// `context` names the session (the purpose is set by the caller).
    #[must_use]
    pub fn new(caller: Arc<AuxCaller>, route: AuxRoute, context: CallContext) -> Self {
        Self {
            caller,
            route,
            context,
        }
    }
}

impl Describer for AuxDescriber {
    fn describe<'a>(&'a self, image: &'a ImageData, question: &'a str) -> DescribeFuture<'a> {
        Box::pin(async move {
            let request = Request {
                model: self.route.route.model.clone(),
                system: vec![SystemBlock {
                    text: DESCRIBE_SYSTEM.to_owned(),
                    cache_breakpoint: false,
                }],
                messages: vec![Message {
                    role: Role::User,
                    parts: vec![
                        Part::Image {
                            media_type: image.mime.clone(),
                            data: image.base64(),
                            is_url: false,
                        },
                        Part::Text {
                            text: question.to_owned(),
                        },
                    ],
                    cache_breakpoint: false,
                }],
                ..Request::default()
            };
            self.caller
                .call(&self.route, &request, &self.context)
                .await
                .map(|reply| reply.text)
                .map_err(|err| err.message().to_owned())
        })
    }
}

/// The request part of `image` (called `name` in notes) for `delivery`:
/// pixels made acceptable to a raster-only route, or the `vision`
/// auxiliary description. Never fails: an image that cannot be shown
/// becomes a note, so it cannot fail the whole request.
pub async fn image_part(
    image: ImageData,
    name: &str,
    delivery: Delivery,
    describer: Option<&dyn Describer>,
) -> Part {
    match delivery {
        Delivery::Native => match inline(image, name) {
            Inline::Image(image) => Part::Image {
                data: image.base64(),
                media_type: image.mime,
                is_url: false,
            },
            Inline::Note(note) => Part::Text { text: note },
        },
        Delivery::Text => {
            let text = match describer {
                None => format!("[image {name}: no vision route can describe it]"),
                Some(describer) => match describer.describe(&image, DESCRIBE_QUESTION).await {
                    Ok(description) => format!("[image {name}: {description}]"),
                    Err(why) => format!("[image {name} could not be described: {why}]"),
                },
            };
            Part::Text { text }
        }
    }
}
