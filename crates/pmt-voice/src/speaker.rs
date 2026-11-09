//! Spoken replies while they generate (spec: voice-mode, Sentence-streamed
//! spoken replies). A [`Mouth`] takes the reply's text deltas, cuts them
//! into speakable sentences ([`Sentences`]) and hands each to a
//! synthesizer task; a player task plays the clips strictly in order, one
//! at a time. The next sentence is synthesized while the previous one
//! plays, so the first sentence is heard while the model is still
//! writing the rest.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::device::{Clip, Player, Synth};
use crate::speakable::Sentences;

/// Clips synthesized ahead of the one playing.
const AHEAD: usize = 2;

/// Speaks one reply.
pub struct Mouth {
    sentences: Mutex<Sentences>,
    to_say: Mutex<Option<mpsc::UnboundedSender<String>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Mouth {
    /// Starts the synthesizer and player tasks of one reply.
    #[must_use]
    pub fn start(synth: Arc<dyn Synth>, player: Arc<dyn Player>) -> Arc<Self> {
        let (to_say, mut said) = mpsc::unbounded_channel::<String>();
        let (to_play, mut playing) = mpsc::channel::<Clip>(AHEAD);
        let synthesize = tokio::spawn(async move {
            while let Some(sentence) = said.recv().await {
                match synth.synthesize(&sentence).await {
                    Ok(sound) => {
                        let clip = Clip {
                            label: sentence,
                            sound,
                        };
                        if to_play.send(clip).await.is_err() {
                            return;
                        }
                    }
                    Err(why) => tracing::warn!(%why, "a spoken sentence was not synthesized"),
                }
            }
        });
        let play = tokio::spawn(async move {
            while let Some(clip) = playing.recv().await {
                if let Err(why) = player.play(clip).await {
                    tracing::warn!(%why, "a spoken sentence was not played");
                }
            }
        });
        Arc::new(Self {
            sentences: Mutex::new(Sentences::new()),
            to_say: Mutex::new(Some(to_say)),
            tasks: Mutex::new(vec![synthesize, play]),
        })
    }

    /// More reply text.
    pub fn text(&self, delta: &str) {
        let ready = self
            .sentences
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(delta);
        self.say(ready);
    }

    fn say(&self, sentences: Vec<String>) {
        if let Some(to_say) = &*self.to_say.lock().unwrap_or_else(PoisonError::into_inner) {
            for sentence in sentences {
                let _ = to_say.send(sentence);
            }
        }
    }

    /// The reply is complete: speaks what is left and waits until every
    /// sentence has played.
    pub async fn finish(&self) {
        let rest = self
            .sentences
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finish();
        self.say(rest);
        self.to_say
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Stops speaking now: nothing more is synthesized or played.
    pub fn stop(&self) {
        self.to_say
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        for task in self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            task.abort();
        }
    }
}

impl Drop for Mouth {
    fn drop(&mut self) {
        self.stop();
    }
}
