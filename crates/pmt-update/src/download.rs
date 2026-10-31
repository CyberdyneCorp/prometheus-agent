//! Downloads under the retry budget (spec: distribution-and-update,
//! Download retry budget), shared by release files and runtime archives.
//!
//! Only transient failures are retried: HTTP 408, 429, 500, 502, 503 and
//! 504, connection errors (resets, refused connections, temporary DNS
//! failures) and timeouts. The delay doubles from [`Budget::base`] up to
//! [`Budget::cap`], and the download gives up after
//! [`Budget::max_stalled`] consecutive failures that made no byte progress.
//! A retry resumes with a `Range` request from the bytes already written;
//! a server that ignores the range answers 200 and the sink starts over.

use std::fs::File;
use std::io::{self, Seek as _, Write as _};
use std::path::Path;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::RANGE;

/// The retry budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Consecutive failures without progress before giving up.
    pub max_stalled: u32,
    /// The first delay.
    pub base: Duration,
    /// The longest delay.
    pub cap: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_stalled: 4,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

impl Budget {
    /// The delay before retry number `stalled` (1-based) of a run of
    /// failures without progress.
    #[must_use]
    pub fn delay(&self, stalled: u32) -> Duration {
        let factor = 1u32 << stalled.saturating_sub(1).min(16);
        self.base.saturating_mul(factor).min(self.cap)
    }
}

/// What a finished download took.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Retries after transient failures.
    pub retries: u32,
    /// Whether any request resumed earlier bytes with a range request.
    pub resumed: bool,
}

/// Where downloaded bytes go.
pub trait Sink: Send {
    /// Bytes written so far.
    fn written(&self) -> u64;
    /// Appends `bytes`.
    ///
    /// # Errors
    ///
    /// The I/O error.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Drops everything written (the server restarted the body).
    ///
    /// # Errors
    ///
    /// The I/O error.
    fn reset(&mut self) -> io::Result<()>;
}

impl Sink for Vec<u8> {
    fn written(&self) -> u64 {
        self.len() as u64
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }

    fn reset(&mut self) -> io::Result<()> {
        self.clear();
        Ok(())
    }
}

/// A partial-download file, appended to across retries and runs.
#[derive(Debug)]
pub struct FileSink {
    file: File,
    written: u64,
}

impl FileSink {
    /// Opens (or creates) `path`, keeping the bytes an earlier run left.
    ///
    /// # Errors
    ///
    /// The I/O error.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::options()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        let written = file.seek(io::SeekFrom::End(0))?;
        Ok(Self { file, written })
    }

    /// Flushes the bytes to disk.
    ///
    /// # Errors
    ///
    /// The I/O error.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

impl Sink for FileSink {
    fn written(&self) -> u64 {
        self.written
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn reset(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.written = 0;
        Ok(())
    }
}

enum Failure {
    Transient(String),
    Permanent(String),
}

/// Downloads `url` into `sink` (continuing after the bytes it holds).
///
/// # Errors
///
/// A one-line reason: the permanent failure, or the last transient one
/// once the budget is spent.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    sink: &mut dyn Sink,
    budget: &Budget,
) -> Result<Stats, String> {
    let mut stats = Stats::default();
    let mut stalled = 0;
    loop {
        let before = sink.written();
        match attempt(client, url, sink, &mut stats).await {
            Ok(()) => return Ok(stats),
            Err(Failure::Permanent(reason)) => return Err(reason),
            Err(Failure::Transient(reason)) => {
                stalled = if sink.written() > before {
                    1
                } else {
                    stalled + 1
                };
                if stalled >= budget.max_stalled {
                    return Err(format!(
                        "{reason} ({stalled} consecutive failures without progress)"
                    ));
                }
                stats.retries += 1;
                tracing::debug!(component = "update", url, retry = stats.retries, "{reason}");
                tokio::time::sleep(budget.delay(stalled)).await;
            }
        }
    }
}

async fn attempt(
    client: &reqwest::Client,
    url: &str,
    sink: &mut dyn Sink,
    stats: &mut Stats,
) -> Result<(), Failure> {
    let offset = sink.written();
    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    let mut response = request.send().await.map_err(|e| request_failure(&e))?;
    let code = response.status();
    let local = |e: io::Error| Failure::Permanent(format!("cannot write the download: {e}"));
    match code {
        StatusCode::PARTIAL_CONTENT if offset > 0 => stats.resumed = true,
        StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
            // The partial file does not fit the remote one: start over.
            sink.reset().map_err(local)?;
            return Err(Failure::Transient(format!("HTTP {code}")));
        }
        s if s.is_success() => sink.reset().map_err(local)?,
        s if is_transient(s) => return Err(Failure::Transient(format!("HTTP {s}"))),
        s => return Err(Failure::Permanent(format!("HTTP {s}"))),
    }
    loop {
        match response.chunk().await {
            Ok(Some(bytes)) => sink.append(&bytes).map_err(local)?,
            Ok(None) => return Ok(()),
            Err(e) => return Err(Failure::Transient(e.to_string())),
        }
    }
}

fn is_transient(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

/// Connection failures, resets and timeouts are transient; a request that
/// cannot be built (bad URL, bad proxy) is not.
fn request_failure(e: &reqwest::Error) -> Failure {
    if e.is_builder() {
        Failure::Permanent(e.to_string())
    } else {
        Failure::Transient(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Budget;

    #[test]
    fn delay_doubles_up_to_the_cap() {
        let budget = Budget::default();
        assert_eq!(budget.delay(1), Duration::from_millis(500));
        assert_eq!(budget.delay(2), Duration::from_secs(1));
        assert_eq!(budget.delay(3), Duration::from_secs(2));
        assert_eq!(budget.delay(40), Duration::from_secs(30));
    }
}
