//! What a running session sees of its grants: `grants.toml` re-read
//! whenever the file changes (so `prometheus grants revoke` reaches the
//! next tool call of a running session, and a hand-edited `[approvals]`
//! list the next approval check), plus session-only grants that live in
//! memory and are discarded with the session.

use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use pmt_core::capability::{Capability, CapabilitySet, not_granted};
use pmt_core::error::Error;

use super::{Grants, GrantsFile, Subject};

/// The identity of one version of the file: an atomic replace makes a new
/// file (new inode on POSIX), and any edit changes the modification time or
/// the length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

fn stamp(file: &GrantsFile) -> Option<Stamp> {
    let meta = std::fs::symlink_metadata(file.path()).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        inode,
    })
}

/// `grants.toml`, cached and re-read when it changes.
#[derive(Debug)]
pub struct LiveGrants {
    file: GrantsFile,
    cache: Mutex<Option<(Option<Stamp>, Grants)>>,
}

impl LiveGrants {
    /// A live view of `file` (read on first use).
    #[must_use]
    pub fn new(file: GrantsFile) -> Self {
        Self {
            file,
            cache: Mutex::new(None),
        }
    }

    /// The file.
    #[must_use]
    pub fn file(&self) -> &GrantsFile {
        &self.file
    }

    /// The current contents, re-read when the file changed since last use.
    ///
    /// # Errors
    ///
    /// As [`GrantsFile::load`].
    pub fn current(&self) -> Result<Grants, Error> {
        let now = stamp(&self.file);
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((seen, grants)) = cache.as_ref()
            && *seen == now
        {
            return Ok(grants.clone());
        }
        let grants = self.file.load()?;
        *cache = Some((now, grants.clone()));
        Ok(grants)
    }
}

/// One session's grants: the profile baseline, the live persistent grants
/// and the session-only grants answered with `session`.
#[derive(Debug)]
pub struct SessionGrants {
    baseline: CapabilitySet,
    persistent: LiveGrants,
    session: Mutex<Vec<(Subject, Capability)>>,
}

impl SessionGrants {
    /// A session over `baseline` (see [`super::baseline`]) and `file`.
    #[must_use]
    pub fn new(baseline: CapabilitySet, file: GrantsFile) -> Self {
        Self {
            baseline,
            persistent: LiveGrants::new(file),
            session: Mutex::new(Vec::new()),
        }
    }

    /// The persistent grants (also the writer for `always` answers).
    #[must_use]
    pub fn persistent(&self) -> &LiveGrants {
        &self.persistent
    }

    /// What `subject` holds now: its persistent and session grants, and for
    /// [`Subject::Profile`] (the session itself) the baseline too.
    ///
    /// # Errors
    ///
    /// As [`GrantsFile::load`].
    pub fn capabilities(&self, subject: &Subject) -> Result<CapabilitySet, Error> {
        let persistent = self.persistent.current()?.capabilities(subject);
        let session: CapabilitySet = self
            .lock()
            .iter()
            .filter(|(s, _)| s == subject)
            .map(|(_, c)| c.clone())
            .collect();
        let held = persistent.union(&session);
        Ok(match subject {
            Subject::Profile => self.baseline.union(&held),
            _ => held,
        })
    }

    /// `Ok` when `subject` holds `capability` now.
    ///
    /// # Errors
    ///
    /// `capability.not_granted`: `capability not granted: <capability>`;
    /// as [`GrantsFile::load`].
    pub fn require(&self, subject: &Subject, capability: &Capability) -> Result<(), Error> {
        if self.capabilities(subject)?.allows(capability) {
            Ok(())
        } else {
            Err(not_granted(capability))
        }
    }

    /// Grants `capabilities` to `subject` until the session ends.
    pub fn grant_for_session(&self, subject: &Subject, capabilities: &CapabilitySet) {
        self.lock()
            .extend(capabilities.iter().map(|c| (subject.clone(), c.clone())));
    }

    /// `[approvals].allow`, as on disk now.
    ///
    /// # Errors
    ///
    /// As [`GrantsFile::load`].
    pub fn allowed_commands(&self) -> Result<Vec<String>, Error> {
        Ok(self.persistent.current()?.allow)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(Subject, Capability)>> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
