//! Composing the next message (spec: cli, Image attachments; External
//! editor composition): `/image` attachments that ride the next prompt,
//! and `/prompt` (aliases `/compose`, `/edit`) or Ctrl+X Ctrl+E, which
//! open `$VISUAL`, `$EDITOR` or the platform editor on a private Markdown
//! file and send what was saved.

use std::fmt::Write as _;
use std::path::PathBuf;

use pmt_agent::turn::TurnInput;
use pmt_core::error::{Error, codes};
use pmt_core::id::MessageId;
use pmt_core::private_fs;

use super::{Flow, Repl};
use crate::chat::attach;
use crate::config_cmd::edit::{DEFAULT_EDITOR, launch_editor};

impl Repl {
    /// `/image <path>` attaches, bare `/image` lists, `/image clear` drops.
    pub(super) fn image(&mut self, args: &str) -> Result<String, Error> {
        let strings = self.strings();
        match args.trim() {
            "" if self.attachments.is_empty() => Ok(pmt_i18n::t!(strings, "cli.no_images")),
            "" => {
                let mut text = pmt_i18n::t!(strings, "cli.images_heading");
                for image in &self.attachments {
                    let _ = write!(text, "\n  {}", image.path.display());
                }
                Ok(text)
            }
            "clear" => {
                let n = self.attachments.len();
                self.attachments.clear();
                Ok(pmt_i18n::t!(strings, "cli.images_dropped", count = n))
            }
            path => {
                let image = attach::load(&resolve(path, &self.factory.options.cwd))?;
                let line = pmt_i18n::t!(
                    strings,
                    "cli.image_attached",
                    name = image.name,
                    count = self.attachments.len() + 1
                );
                self.attachments.push(image);
                Ok(line)
            }
        }
    }

    /// The input of a user turn: `text` with the pending attachments
    /// (cleared once they are stored).
    pub(super) async fn compose_input(&mut self, text: String) -> Result<TurnInput, Error> {
        let mut input = TurnInput::text(text);
        if self.attachments.is_empty() {
            return Ok(input);
        }
        let images = attach::prepare(&self.factory, self.live(), &self.attachments).await?;
        self.attachments.clear();
        input.images = images;
        Ok(input)
    }

    /// `/prompt [initial text]` or Ctrl+X Ctrl+E: the editor's result is
    /// the next user message.
    pub(super) async fn compose(&mut self, initial: &str) -> Flow {
        match self.edit_message(initial) {
            Ok(Some(text)) => self.user_turn(text).await,
            Ok(None) => self.notice(self.strings().get("cli.empty_prompt")),
            Err(err) => self.error(&err),
        }
        Flow::Continue
    }

    /// Opens the editor on a private temporary file holding `initial`;
    /// `None` when the saved file is empty (or still the empty start).
    fn edit_message(&self, initial: &str) -> Result<Option<String>, Error> {
        let editor = self.factory.launch().editor().unwrap_or(DEFAULT_EDITOR);
        // The editor takes the terminal: earlier output first.
        self.input.flush();
        edit_message(&self.factory.scope.path("cache/compose"), editor, initial)
    }
}

/// A relative path against the session's working directory.
fn resolve(path: &str, cwd: &std::path::Path) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Opens `editor` on a private file in `dir` holding `initial`; `None`
/// when the saved file is empty (or still the empty start). The caller
/// flushes its output first: the editor takes the terminal.
///
/// # Errors
///
/// `home.io` when the file cannot be prepared; the editor's failure.
pub(crate) fn edit_message(
    dir: &std::path::Path,
    editor: &str,
    initial: &str,
) -> Result<Option<String>, Error> {
    let io = |e: std::io::Error| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot prepare the message file: {e}"),
        )
    };
    private_fs::create_dir(dir).map_err(io)?;
    let path = dir.join(format!("prompt-{}.md", MessageId::new().ulid()));
    private_fs::write_atomic(&path, initial.as_bytes()).map_err(io)?;
    let result =
        launch_editor(editor, &path).and_then(|()| std::fs::read_to_string(&path).map_err(io));
    let _ = std::fs::remove_file(&path);
    let text = result?;
    Ok((!text.trim().is_empty()).then_some(text))
}
