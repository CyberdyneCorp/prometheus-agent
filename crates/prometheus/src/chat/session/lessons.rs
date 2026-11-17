//! The experience lessons a session starts with (spec: experience-library,
//! Lessons offered to later sessions; Library events). A top-level
//! session with a known domain — `--domain`, `/domain`, or the scheduled
//! job or kanban task that started it — gets, when
//! `experience.session_lessons` is on, one prompt layer listing its
//! domain's best lessons. The layer is decided once, at the session's
//! first open, and stored on its row: a resumed session or a prompt
//! rebuild reads it back, so a lesson adopted while the session is open
//! only reaches the next session (prompt-cache invariant).

use pmt_core::id::SessionId;
use pmt_evolve::experience::{domain, ops};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::LessonsInjected;

use super::{Factory, Live};

/// What a session's lessons layer is.
#[derive(Debug, Clone, Default)]
pub(super) struct Lessons {
    /// The layer text, if any.
    pub layer: Option<String>,
    /// The entries injected at this open (empty when the layer was
    /// decided earlier).
    pub injected: Vec<String>,
}

impl Factory {
    /// Tags session `id` with this process's `--domain` when it has none
    /// yet (a top-level session only).
    fn tag_domain(&self, id: SessionId) {
        let Some(name) = &self.options.domain else {
            return;
        };
        let Ok(name) = domain::parse(name) else {
            return;
        };
        if matches!(self.store.session_domain(id), Ok(None))
            && let Err(err) = self.store.set_session_domain(id, &name)
        {
            tracing::warn!(session = %id, error = %err, "session domain not recorded");
        }
    }

    /// The lessons layer of session `id`: read back when already decided,
    /// else decided now (and each injected entry's `times_shown` counted).
    pub(super) fn lessons_for(&self, id: SessionId, child: bool) -> Lessons {
        if child {
            return Lessons::default();
        }
        self.tag_domain(id);
        if let Ok(Some(frozen)) = self.store.session_lessons(id) {
            return Lessons {
                layer: (!frozen.is_empty()).then_some(frozen),
                injected: Vec::new(),
            };
        }
        let config = self.scope.config();
        let decided = match self.store.session_domain(id) {
            Ok(Some(name)) if config.evolve.enabled && config.experience.session_lessons => {
                let max = usize::try_from(config.experience.session_lessons_max).unwrap_or(5);
                ops::session_lessons(self.scope.dir(), &name, max)
            }
            _ => None,
        };
        let text = decided.as_ref().map_or("", |(t, _)| t.as_str());
        if let Err(err) = self.store.set_session_lessons(id, text) {
            tracing::warn!(session = %id, error = %err, "session lessons not frozen");
            return Lessons::default();
        }
        match decided {
            Some((layer, ids)) => {
                ops::shown(self.scope.dir(), &ids);
                Lessons {
                    layer: Some(layer),
                    injected: ids,
                }
            }
            None => Lessons::default(),
        }
    }
}

impl Live {
    /// `experience.session_lessons.injected` for the entries `ids`.
    pub(super) async fn emit_lessons(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        self.emit(Event::ExperienceSessionLessonsInjected(LessonsInjected {
            session: self.id.to_string(),
            entry_ids: ids.to_vec(),
        }))
        .await;
    }
}
