//! Generating a pet (spec: pets, Generating a pet): `/hatch <description>`
//! and the desktop generate flow. Two paid steps, each shown with its
//! estimated cost and run only after the user agrees: up to four base
//! drafts per round (pick one, remix with a tweak, or retry), then one
//! animation row per state with the chosen draft as reference image,
//! packed into a valid atlas and installed like a gallery pet.
//!
//! [`Hatch`] is the conversation, shared by every surface: it is started
//! with a description and fed the user's answers; each step says what to
//! show and whether an answer is awaited. The image backend is a
//! [`Backend`] (the binary adapts the profile's image provider).

pub mod pack;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use image::RgbaImage;
use pmt_core::error::{Error, codes};

use crate::store::{PetMeta, Pets, slugify};

/// Drafts per round.
pub const DRAFTS: u32 = 4;

/// The message when no backend can generate a pet.
pub const NO_BACKEND: &str = "pet generation needs an image backend with reference-image support; configure one with 'prometheus tools'";

/// `pet.no_image_backend`, naming why the configured backend is unfit
/// (`reason`: the backend's own error).
#[must_use]
pub fn no_backend(reason: Option<&str>) -> Error {
    let message = match reason {
        Some(reason) => format!("{NO_BACKEND} ({reason})"),
        None => NO_BACKEND.to_owned(),
    };
    Error::new(&codes::PET_NO_IMAGE_BACKEND, message)
}

/// What the backend is and costs.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendInfo {
    /// The provider id.
    pub provider: String,
    /// The model.
    pub model: String,
    /// The estimated price of one image (unknown: `None`).
    pub usd_per_image: Option<f64>,
}

impl BackendInfo {
    /// The estimate of `images` images, as shown before a paid step.
    #[must_use]
    pub fn estimate(&self, images: u32) -> String {
        match self.usd_per_image {
            Some(each) => format!(
                "about ${:.2} ({images} images with {}/{})",
                each * f64::from(images),
                self.provider,
                self.model
            ),
            None => format!(
                "{images} images with {}/{} (price unknown; billed by {})",
                self.provider, self.model, self.provider
            ),
        }
    }
}

/// A future of a backend call.
pub type Generation<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// An image backend that takes reference images.
pub trait Backend: Send + Sync {
    /// What it is and costs.
    fn info(&self) -> BackendInfo;
    /// One image for `prompt`, with `reference` (PNG bytes) when given.
    fn generate<'a>(&'a self, prompt: &'a str, reference: Option<&'a [u8]>) -> Generation<'a>;
}

/// The prompt of one base draft.
#[must_use]
pub fn draft_prompt(description: &str, tweak: &str) -> String {
    let tweak = if tweak.is_empty() {
        String::new()
    } else {
        format!(" Adjustment: {tweak}.")
    };
    format!(
        "A cute pixel-art mascot character: {description}.{tweak} Full body, centred, facing the viewer, \
         clean outline, on a flat pure magenta (#ff00ff) background with no shadow or scenery."
    )
}

/// The prompt of one animation row.
#[must_use]
pub fn row_prompt(description: &str, pose: &str) -> String {
    format!(
        "Sprite sheet row of the exact character in the reference image ({description}): eight frames side by \
         side, evenly spaced, of the character {pose}, same size and style in every frame, on a flat pure \
         magenta (#ff00ff) background."
    )
}

/// What a surface shows next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Show `text` and wait for an answer.
    Ask(String),
    /// Show `text`; the flow is over (`slug`: the pet installed).
    Done {
        /// What to show.
        text: String,
        /// The installed pet.
        slug: Option<String>,
        /// The backend or the packing failed.
        failed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    ConfirmDrafts,
    Pick,
    ConfirmRows(usize),
    Over,
}

/// One generation conversation.
pub struct Hatch {
    description: String,
    tweak: String,
    backend: Arc<dyn Backend>,
    pets: Pets,
    drafts_dir: PathBuf,
    drafts: Vec<Vec<u8>>,
    stage: Stage,
}

fn decode(bytes: &[u8]) -> Result<RgbaImage, String> {
    image::load_from_memory(bytes)
        .map(image::DynamicImage::into_rgba8)
        .map_err(|e| format!("the backend returned no readable image: {e}"))
}

impl Hatch {
    /// Starts generating `description`; drafts are written to
    /// `drafts_dir` for the user to look at.
    #[must_use]
    pub fn start(
        description: &str,
        backend: Arc<dyn Backend>,
        pets: Pets,
        drafts_dir: PathBuf,
    ) -> (Self, Step) {
        let hatch = Self {
            description: description.trim().to_owned(),
            tweak: String::new(),
            backend,
            pets,
            drafts_dir,
            drafts: Vec::new(),
            stage: Stage::ConfirmDrafts,
        };
        let step = hatch.confirm_drafts();
        (hatch, step)
    }

    /// The description being generated.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The drafts of the last round (PNG or the backend's format).
    #[must_use]
    pub fn drafts(&self) -> &[Vec<u8>] {
        &self.drafts
    }

    /// Whether the flow is over.
    #[must_use]
    pub fn over(&self) -> bool {
        self.stage == Stage::Over
    }

    fn confirm_drafts(&self) -> Step {
        Step::Ask(format!(
            "Hatch \"{}\": first step, {DRAFTS} base drafts, {}. Generate? [y/N]",
            self.description,
            self.backend.info().estimate(DRAFTS)
        ))
    }

    fn cancel(&mut self) -> Step {
        self.stage = Stage::Over;
        Step::Done {
            text: "pet generation cancelled; nothing more was spent".to_owned(),
            slug: None,
            failed: false,
        }
    }

    /// Feeds the user's answer.
    pub async fn answer(&mut self, answer: &str) -> Step {
        let answer = answer.trim();
        if matches!(answer, "q" | "quit" | "cancel") {
            return self.cancel();
        }
        match self.stage.clone() {
            Stage::ConfirmDrafts if yes(answer) => self.make_drafts().await,
            Stage::ConfirmDrafts => self.cancel(),
            Stage::Pick => self.pick(answer),
            Stage::ConfirmRows(index) if yes(answer) => self.make_rows(index).await,
            Stage::ConfirmRows(_) => {
                self.stage = Stage::Pick;
                Step::Ask(self.pick_question())
            }
            Stage::Over => Step::Done {
                text: String::new(),
                slug: None,
                failed: false,
            },
        }
    }

    fn pick(&mut self, answer: &str) -> Step {
        if let Ok(n) = answer.parse::<usize>()
            && (1..=self.drafts.len()).contains(&n)
        {
            self.stage = Stage::ConfirmRows(n - 1);
            return Step::Ask(format!(
                "Second step: {} animation rows from draft {n}, {}. Generate? [y/N]",
                pack::ROW_POSES.len(),
                self.backend.info().estimate(crate::atlas::ROWS)
            ));
        }
        if answer == "retry" {
            self.stage = Stage::ConfirmDrafts;
            return self.confirm_drafts();
        }
        if let Some(tweak) = answer
            .strip_prefix("remix ")
            .or_else(|| answer.strip_prefix("r "))
        {
            tweak.trim().clone_into(&mut self.tweak);
            self.stage = Stage::ConfirmDrafts;
            return self.confirm_drafts();
        }
        Step::Ask(self.pick_question())
    }

    fn pick_question(&self) -> String {
        let files: Vec<String> = (1..=self.drafts.len())
            .map(|n| format!("  {n}) {}", self.draft_path(n).display()))
            .collect();
        format!(
            "Drafts:\n{}\nPick one (1-{}), 'remix <tweak>', 'retry' or 'cancel':",
            files.join("\n"),
            self.drafts.len()
        )
    }

    fn draft_path(&self, n: usize) -> PathBuf {
        self.drafts_dir
            .join(format!("{}-draft-{n}.png", slugify(&self.description)))
    }

    async fn make_drafts(&mut self) -> Step {
        let prompt = draft_prompt(&self.description, &self.tweak);
        let mut drafts = Vec::new();
        for n in 1..=DRAFTS {
            match self.backend.generate(&prompt, None).await {
                Ok(bytes) => drafts.push(bytes),
                Err(why) => return self.failed(&format!("draft {n}"), &why, drafts.len()),
            }
        }
        if let Err(why) = self.save_drafts(&drafts) {
            return self.failed("saving the drafts", &why, drafts.len());
        }
        self.drafts = drafts;
        self.stage = Stage::Pick;
        Step::Ask(self.pick_question())
    }

    fn save_drafts(&self, drafts: &[Vec<u8>]) -> Result<(), String> {
        std::fs::create_dir_all(&self.drafts_dir).map_err(|e| e.to_string())?;
        for (n, bytes) in (1..).zip(drafts) {
            let image = decode(bytes)?;
            image
                .save_with_format(self.draft_path(n), image::ImageFormat::Png)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn make_rows(&mut self, index: usize) -> Step {
        let reference = match decode(&self.drafts[index]).and_then(|i| png(&i)) {
            Ok(png) => png,
            Err(why) => return self.failed("reading the draft", &why, 0),
        };
        let mut rows = Vec::new();
        for (name, pose) in pack::ROW_POSES {
            let prompt = row_prompt(&self.description, pose);
            let generated = self.backend.generate(&prompt, Some(&reference)).await;
            match generated.and_then(|bytes| decode(&bytes)) {
                Ok(row) => rows.push(row),
                Err(why) => return self.failed(&format!("the {name} row"), &why, rows.len()),
            }
        }
        self.install(&rows)
    }

    fn install(&mut self, rows: &[RgbaImage]) -> Step {
        self.stage = Stage::Over;
        let packed = pack::pack(rows).map_err(|e| e.to_string());
        let slug = self.free_slug();
        let meta = PetMeta {
            slug: slug.clone(),
            name: self.description.clone(),
            author: "generated".to_owned(),
            source: "generated".to_owned(),
            sha256: String::new(),
            license: String::new(),
        };
        let installed = packed.and_then(|(_, png)| {
            self.pets
                .install(&meta, &png, false)
                .map_err(|e| e.message().to_owned())
        });
        match installed {
            Ok(_) => Step::Done {
                text: format!(
                    "hatched '{slug}': installed in {}; adopt it with /pet {slug}",
                    self.pets.dir().join(&slug).display()
                ),
                slug: Some(slug),
                failed: false,
            },
            Err(why) => Step::Done {
                text: format!("pet generation failed while packing the atlas: {why}"),
                slug: None,
                failed: true,
            },
        }
    }

    fn free_slug(&self) -> String {
        let base = slugify(&self.description);
        (1..=1000)
            .map(|n| {
                if n == 1 {
                    base.clone()
                } else {
                    format!("{base}-{n}")
                }
            })
            .find(|slug| !self.pets.dir().join(slug).exists())
            .unwrap_or(base)
    }

    fn failed(&mut self, step: &str, why: &str, made: usize) -> Step {
        self.stage = Stage::Over;
        Step::Done {
            text: format!(
                "pet generation failed at {step} ({made} image(s) generated by {} before it): {why}",
                self.backend.info().provider
            ),
            slug: None,
            failed: true,
        }
    }
}

fn yes(answer: &str) -> bool {
    matches!(answer.to_lowercase().as_str(), "y" | "yes")
}

fn png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use image::Rgba;

    use super::*;

    /// A backend that draws a green body on magenta and records prompts.
    struct Painter {
        prompts: Mutex<Vec<(String, bool)>>,
    }

    impl Backend for Painter {
        fn info(&self) -> BackendInfo {
            BackendInfo {
                provider: "fake".into(),
                model: "m".into(),
                usd_per_image: Some(0.04),
            }
        }

        fn generate<'a>(&'a self, prompt: &'a str, reference: Option<&'a [u8]>) -> Generation<'a> {
            self.prompts
                .lock()
                .unwrap()
                .push((prompt.to_owned(), reference.is_some()));
            Box::pin(async {
                let image = RgbaImage::from_fn(800, 104, |x, _| {
                    if x % 100 < 50 {
                        Rgba([20, 160, 60, 255])
                    } else {
                        Rgba([255, 0, 255, 255])
                    }
                });
                png(&image)
            })
        }
    }

    /// Scenario: Hatched pet is a normal pet.
    #[tokio::test]
    async fn a_hatched_pet_is_installed_and_selectable() {
        let profile = tempfile::tempdir().unwrap();
        let pets = Pets::new(profile.path());
        let painter = Arc::new(Painter {
            prompts: Mutex::new(Vec::new()),
        });
        let (mut hatch, first) = Hatch::start(
            "a sleepy fox",
            painter.clone(),
            pets.clone(),
            profile.path().join("cache/pets/hatch"),
        );
        let Step::Ask(text) = first else { panic!() };
        assert!(text.contains("about $0.16"), "{text}");
        let Step::Ask(pick) = hatch.answer("y").await else {
            panic!()
        };
        assert!(pick.contains("a-sleepy-fox-draft-4.png"), "{pick}");
        let Step::Ask(confirm) = hatch.answer("2").await else {
            panic!()
        };
        assert!(confirm.contains("about $0.36"), "{confirm}");
        let Step::Done { slug, .. } = hatch.answer("yes").await else {
            panic!()
        };
        assert_eq!(slug.as_deref(), Some("a-sleepy-fox"));
        let pet = pets.get("a-sleepy-fox").unwrap();
        pet.load_atlas().unwrap();
        let prompts = painter.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 4 + 9);
        assert!(prompts[4..].iter().all(|(_, reference)| *reference));
    }

    #[tokio::test]
    async fn declining_spends_nothing() {
        let profile = tempfile::tempdir().unwrap();
        let painter = Arc::new(Painter {
            prompts: Mutex::new(Vec::new()),
        });
        let (mut hatch, _) = Hatch::start(
            "x",
            painter.clone(),
            Pets::new(profile.path()),
            profile.path().join("d"),
        );
        assert!(matches!(
            hatch.answer("n").await,
            Step::Done { slug: None, .. }
        ));
        assert!(painter.prompts.lock().unwrap().is_empty());
        assert!(hatch.over());
    }
}
