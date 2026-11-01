//! The loopback listener of a browser sign-in (spec: model-providers,
//! Device-code and browser PKCE sign-in flows): bound to the literal
//! `127.0.0.1`, it answers every request until the genuine callback
//! arrives — 404 off the callback path, 400 when `state` does not match
//! (the code of such a request is never used) — and returns the code.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, timeout, timeout_at};

use super::pkce::constant_time_eq;

/// Longest request head read from one connection.
const MAX_HEAD: usize = 16 * 1024;
/// How long one connection may take to send its request head.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A bound callback listener.
#[derive(Debug)]
pub struct Listener {
    inner: TcpListener,
    port: u16,
}

/// What the genuine callback carries.
#[derive(Debug)]
pub struct Expect<'a> {
    /// The callback path.
    pub path: &'a str,
    /// The `state` of the authorization request.
    pub state: &'a str,
}

/// How one request was answered.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The genuine callback with its code.
    Code(String),
    /// The genuine callback reporting a refusal.
    Refused(String),
    /// Anything else (answered 400 or 404): keep waiting.
    Ignored,
}

impl Listener {
    /// Binds `127.0.0.1:<port>` (0: an OS-assigned port).
    ///
    /// # Errors
    ///
    /// `auth.port_busy` when the port is in use, `auth.login_failed` for
    /// other bind failures.
    pub async fn bind(port: u16) -> Result<Self, Error> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let inner = TcpListener::bind(addr).await.map_err(|e| {
            if e.kind() == ErrorKind::AddrInUse {
                Error::new(
                    &codes::AUTH_PORT_BUSY,
                    format!(
                        "port {port} is in use, so the browser sign-in cannot receive its callback"
                    ),
                )
            } else {
                Error::new(
                    &codes::AUTH_LOGIN_FAILED,
                    format!("cannot listen on 127.0.0.1:{port} for the sign-in callback: {e}"),
                )
            }
        })?;
        let port = inner.local_addr().map_or(port, |a| a.port());
        Ok(Self { inner, port })
    }

    /// The bound port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Answers requests until the genuine callback arrives or `deadline`.
    ///
    /// # Errors
    ///
    /// `auth.login_failed` when the provider reports a refusal or nothing
    /// arrives in time.
    pub async fn wait(
        &self,
        expect: &Expect<'_>,
        deadline: Instant,
    ) -> Result<SecretString, Error> {
        loop {
            let Ok(accepted) = timeout_at(deadline, self.inner.accept()).await else {
                return Err(Error::new(
                    &codes::AUTH_LOGIN_FAILED,
                    "no sign-in callback arrived before the timeout",
                ));
            };
            let Ok((stream, _)) = accepted else {
                continue;
            };
            match serve(stream, expect).await {
                Verdict::Code(code) => return Ok(SecretString::new(code)),
                Verdict::Refused(why) => {
                    return Err(Error::new(
                        &codes::AUTH_LOGIN_FAILED,
                        format!("the provider refused the sign-in: {why}"),
                    ));
                }
                Verdict::Ignored => {}
            }
        }
    }
}

/// Reads one request, answers it and judges it.
async fn serve(mut stream: TcpStream, expect: &Expect<'_>) -> Verdict {
    let Some(target) = read_target(&mut stream).await else {
        return Verdict::Ignored;
    };
    let verdict = judge(&target, expect);
    let (status, text) = match &verdict {
        Verdict::Code(_) => (
            "200 OK",
            "Signed in. You can close this tab and return to Prometheus.",
        ),
        Verdict::Refused(_) => (
            "200 OK",
            "The sign-in was refused. Return to Prometheus for details.",
        ),
        Verdict::Ignored if path_of(&target) == expect.path => {
            ("400 Bad Request", "Invalid sign-in callback.")
        }
        Verdict::Ignored => ("404 Not Found", "Not found."),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    verdict
}

/// The request target of a `GET` request head.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = timeout(READ_TIMEOUT, stream.read(&mut buf))
            .await
            .ok()?
            .ok()?;
        if n == 0 || head.len() + n > MAX_HEAD {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = head.split(|b| *b == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split(' ');
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_owned)
}

fn path_of(target: &str) -> &str {
    target.split('?').next().unwrap_or(target)
}

/// The verdict on request target `target`.
fn judge(target: &str, expect: &Expect<'_>) -> Verdict {
    if path_of(target) != expect.path {
        return Verdict::Ignored;
    }
    let query = target.split_once('?').map_or("", |(_, q)| q);
    let mut state = None;
    let mut code = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match &*key {
            "state" => state = Some(value.into_owned()),
            "code" => code = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }
    let genuine = state.is_some_and(|s| constant_time_eq(s.as_bytes(), expect.state.as_bytes()));
    if !genuine {
        return Verdict::Ignored;
    }
    match (code, error) {
        (_, Some(error)) => Verdict::Refused(description.unwrap_or(error)),
        (Some(code), None) if !code.is_empty() => Verdict::Code(code),
        _ => Verdict::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECT: Expect<'static> = Expect {
        path: "/callback",
        state: "s3cr3t",
    };

    #[test]
    fn only_the_matching_state_on_the_callback_path_counts() {
        assert_eq!(
            judge("/callback?code=abc&state=s3cr3t", &EXPECT),
            Verdict::Code("abc".to_owned())
        );
        assert_eq!(
            judge("/callback?code=abc&state=forged", &EXPECT),
            Verdict::Ignored
        );
        assert_eq!(judge("/callback?code=abc", &EXPECT), Verdict::Ignored);
        assert_eq!(
            judge("/other?code=abc&state=s3cr3t", &EXPECT),
            Verdict::Ignored
        );
        assert_eq!(
            judge("/callback?error=access_denied&state=s3cr3t", &EXPECT),
            Verdict::Refused("access_denied".to_owned())
        );
    }

    async fn get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nhost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn forged_callback_is_answered_400_and_the_wait_goes_on() {
        let listener = Listener::bind(0).await.unwrap();
        let port = listener.port();
        let browser = tokio::spawn(async move {
            let forged = get(port, "/callback?code=evil&state=forged").await;
            let stray = get(port, "/favicon.ico").await;
            let genuine = get(port, "/callback?code=good&state=s3cr3t").await;
            (forged, stray, genuine)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let code = listener.wait(&EXPECT, deadline).await.unwrap();
        assert_eq!(code.expose(), "good");
        let (forged, stray, genuine) = browser.await.unwrap();
        assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
        assert!(stray.starts_with("HTTP/1.1 404"), "{stray}");
        assert!(genuine.starts_with("HTTP/1.1 200"), "{genuine}");
    }

    #[tokio::test]
    async fn a_busy_port_is_reported_as_such() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let err = Listener::bind(port).await.unwrap_err();
        assert_eq!(err.code().code, "auth.port_busy");
    }
}
