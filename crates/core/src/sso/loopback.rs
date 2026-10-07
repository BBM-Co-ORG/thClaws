//! Minimal HTTP listener for the OIDC redirect URI.
//!
//! Public OIDC clients can't use a real callback URL (no server), so
//! the standard pattern for desktop apps is `http://localhost:<random>/`.
//! We open a `TcpListener` on a kernel-assigned port, register that
//! exact URL with the IdP at authorize time, and the browser delivers
//! the authcode back to us via a GET request.
//!
//! ## Why `localhost` and not `127.0.0.1`
//!
//! Azure / Microsoft Entra ID treats `localhost` and `127.0.0.1` as
//! **different hosts** for redirect-URI matching. Its app-registration
//! UX wants you to register `http://localhost` (no port — Microsoft
//! wildcards the port AND accepts any of `http://localhost`,
//! `http://localhost:<port>`, `http://localhost:<port>/` as
//! equivalent to that registration). So the loopback URI we emit
//! has to use `localhost` literally, even though we bind to the
//! `127.0.0.1` IPv4 address — the browser's libc resolver maps
//! `localhost` → `127.0.0.1` on every default macOS / Linux / Windows
//! config (per RFC 6761).
//!
//! Google is more permissive — it accepts both `localhost` and
//! `127.0.0.1` forms for desktop clients, so the `localhost` choice
//! works for both providers from a single registration.
//!
//! The implementation reads exactly one HTTP request, extracts the
//! `code` and `state` query parameters, returns a small "you can close
//! this tab now" HTML page, and shuts down. ~60 lines of std::net,
//! no extra HTTP-server dependency.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::error::{Error, Result};

/// What the browser delivered to our loopback URL.
#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// A listener bound to `127.0.0.1:<random>`. Hold this across the
/// browser open + token exchange — `accept_one()` consumes it.
pub struct LoopbackServer {
    listener: TcpListener,
    port: u16,
}

impl LoopbackServer {
    /// Bind to a kernel-assigned port on 127.0.0.1. Use `redirect_uri()`
    /// to get the URL to register with the IdP.
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| Error::Tool(format!("loopback bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Tool(format!("loopback local_addr: {e}")))?
            .port();
        Ok(LoopbackServer { listener, port })
    }

    /// The redirect URI to send to the IdP — matches what the browser
    /// will hit when the user finishes login. Uses `localhost` (not
    /// `127.0.0.1`) so the URI lines up with an Azure portal
    /// registration of `http://localhost`; see the module docstring
    /// for the matching rules.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/", self.port)
    }

    /// Accept exactly one HTTP request, parse the query string, return
    /// what the IdP sent us. Times out after `timeout_secs` so a user
    /// who closes their browser doesn't leave thClaws hanging.
    pub fn accept_one(self, timeout_secs: u64) -> Result<CallbackResult> {
        self.listener
            .set_nonblocking(false)
            .map_err(|e| Error::Tool(format!("set_nonblocking: {e}")))?;
        // accept() is blocking — we use a separate timer to give up
        // after timeout_secs.
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            self.listener
                .set_nonblocking(true)
                .map_err(|e| Error::Tool(format!("set_nonblocking: {e}")))?;
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    return handle_one(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        return Err(Error::Tool(format!(
                            "OIDC callback not received within {timeout_secs}s — did you complete login in the browser?"
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(Error::Tool(format!("loopback accept: {e}")));
                }
            }
        }
    }
}

fn handle_one(mut stream: TcpStream) -> Result<CallbackResult> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| Error::Tool(format!("set_read_timeout: {e}")))?;
    // Read just the request line ("GET /callback?code=... HTTP/1.1").
    // Don't bother with headers / body — OIDC redirect is a GET.
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| Error::Tool(format!("read request line: {e}")))?;
    // Drain remaining headers (best-effort) so the browser doesn't
    // think we hung up before reading the request.
    let mut sink = [0u8; 1024];
    let _ = (&stream).read(&mut sink);

    let result = parse_request_line(&request_line);

    // Send a friendly response back to the browser.
    let body = if result.code.is_some() {
        success_html()
    } else {
        error_html(
            result.error.as_deref().unwrap_or("unknown"),
            result.error_description.as_deref().unwrap_or(""),
        )
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    Ok(result)
}

/// Parse the first line of an HTTP request and pull `code` / `state` /
/// `error` query parameters out. Tolerant of missing or malformed
/// inputs — returns empty fields rather than erroring.
pub fn parse_request_line(line: &str) -> CallbackResult {
    let mut result = CallbackResult {
        code: None,
        state: None,
        error: None,
        error_description: None,
    };
    let parts: Vec<&str> = line.split_whitespace().collect();
    let path = match parts.get(1) {
        Some(p) => *p,
        None => return result,
    };
    let query = match path.split_once('?') {
        Some((_, q)) => q,
        None => return result,
    };
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let v = url_decode(v);
        match k {
            "code" => result.code = Some(v),
            "state" => result.state = Some(v),
            "error" => result.error = Some(v),
            "error_description" => result.error_description = Some(v),
            _ => {}
        }
    }
    result
}

/// Minimal `application/x-www-form-urlencoded` decode: `+` → space,
/// `%XX` → byte. Enough for OIDC query parameters which only ever
/// contain URL-safe values plus the occasional space.
fn url_decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte as char);
                    i += 3;
                } else {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Shared page shell for the browser tab the loopback answers — the same
/// light card look as the cloud's sign-in page, with a dark-mode variant.
fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>:root{{--bg:#f4f7f8;--card:#fff;--ink:#0b1f2a;--ink2:#3f5561;--line:#d6e0e4;--ok:#0a7480;--bad:#b4332c;--code:#eaf0f2}}@media(prefers-color-scheme:dark){{:root{{--bg:#0b161c;--card:#0f1c24;--ink:#e6f0f3;--ink2:#9fb3bd;--line:#223540;--ok:#22a9b5;--bad:#e0716b;--code:#16262f}}}}body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:var(--bg);color:var(--ink);font:15px/1.6 system-ui,-apple-system,"Segoe UI",sans-serif;padding:16px;box-sizing:border-box}}main{{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:28px;max-width:420px;width:100%;text-align:center}}.mark{{width:44px;height:44px;border-radius:50%;display:inline-flex;align-items:center;justify-content:center;font-size:22px;color:#fff;margin-bottom:12px}}h1{{font-size:20px;font-weight:600;margin:0 0 6px}}p{{color:var(--ink2);margin:6px 0}}code{{background:var(--code);padding:2px 6px;border-radius:4px;font-size:13px}}</style></head><body><main>{body}</main></body></html>"#
    )
}

fn success_html() -> String {
    page(
        "Signed in",
        r#"<div class="mark" style="background:var(--ok)">✓</div><h1>You're signed in</h1><p>Return to the desktop app — you can close this tab.</p>"#,
    )
}

fn error_html(error: &str, description: &str) -> String {
    page(
        "Sign-in failed",
        &format!(
            r#"<div class="mark" style="background:var(--bad)">!</div><h1>Sign-in didn't finish</h1><p><code>{error}</code></p><p>{description}</p><p>Start the sign-in again from the desktop app.</p>"#
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_callback_with_code_and_state() {
        // Root path — matches the `http://localhost:<port>/` shape
        // we now emit so Azure's `http://localhost` redirect-URI
        // registration accepts the redirect (see module docstring).
        let line = "GET /?code=abc123&state=xyz HTTP/1.1\r\n";
        let r = parse_request_line(line);
        assert_eq!(r.code.as_deref(), Some("abc123"));
        assert_eq!(r.state.as_deref(), Some("xyz"));
        assert!(r.error.is_none());
    }

    #[test]
    fn parses_callback_with_error() {
        let line = "GET /?error=access_denied&error_description=user+cancelled HTTP/1.1\r\n";
        let r = parse_request_line(line);
        assert_eq!(r.error.as_deref(), Some("access_denied"));
        assert_eq!(r.error_description.as_deref(), Some("user cancelled"));
        assert!(r.code.is_none());
    }

    #[test]
    fn parser_tolerates_callback_path_for_back_compat() {
        // Earlier loopback URIs used `/callback`; the parser stays
        // tolerant so any IdP that mirrors back an arbitrary path
        // (or a user with a cached redirect from an older build)
        // still gets parsed correctly.
        let line = "GET /callback?code=abc&state=xyz HTTP/1.1\r\n";
        let r = parse_request_line(line);
        assert_eq!(r.code.as_deref(), Some("abc"));
        assert_eq!(r.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("%2B%2F%3D"), "+/=");
    }

    #[test]
    fn parses_callback_url_decodes_state() {
        let line = "GET /callback?code=foo&state=a%2Bb HTTP/1.1\r\n";
        let r = parse_request_line(line);
        assert_eq!(r.state.as_deref(), Some("a+b"));
    }

    #[test]
    fn empty_query_returns_empty_result() {
        let line = "GET / HTTP/1.1\r\n";
        let r = parse_request_line(line);
        assert!(r.code.is_none());
        assert!(r.state.is_none());
    }

    #[test]
    fn malformed_request_doesnt_panic() {
        let line = "garbage";
        let _ = parse_request_line(line); // no panic
    }

    #[test]
    fn loopback_bind_assigns_port() {
        let server = LoopbackServer::bind().expect("bind");
        assert!(server.port > 0);
        assert!(server.redirect_uri().contains(&server.port.to_string()));
    }

    /// Azure rejects the OAuth authorize request when redirect_uri's
    /// host is `127.0.0.1` but the portal registration uses
    /// `localhost`. Pin the URI shape so a future refactor doesn't
    /// silently re-introduce the regression.
    #[test]
    fn redirect_uri_uses_localhost_host_and_root_path() {
        let server = LoopbackServer::bind().expect("bind");
        let uri = server.redirect_uri();
        assert!(
            uri.starts_with("http://localhost:"),
            "expected localhost host, got: {uri}"
        );
        assert!(uri.ends_with('/'), "expected root path, got: {uri}");
        assert!(
            !uri.contains("127.0.0.1"),
            "must not use 127.0.0.1 (breaks Azure match): {uri}"
        );
        assert!(
            !uri.contains("/callback"),
            "redirect must not carry a /callback suffix (breaks Azure 'http://localhost' registration match): {uri}"
        );
    }
}
