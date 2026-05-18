//! Best-effort webhook delivery on tick completion.
//!
//! Only HTTP/1.1 to a plain TCP host is supported — no TLS, no auth, no
//! retries. The intent is "post a JSON blob to a local listener / a
//! Discord-style URL the user already wrangles via a reverse proxy."
//! Failures are logged and never propagate to the tick loop.

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing::warn;

#[derive(Debug, Clone, Serialize)]
pub struct TickWebhookPayload {
    pub tick_id: String,
    pub started_at_unix: u64,
    pub finished_at_unix: u64,
    pub memory_root: String,
    pub dry_run: bool,
    pub ran: bool,
    pub reason_skipped: Option<String>,
    pub exit_code: Option<i32>,
    pub audit_total_issues: usize,
    pub pre_tick_sha: Option<String>,
}

/// POST a JSON body to `url` with a 5s timeout. Returns Ok even on
/// upstream failure — callers should treat this as fire-and-forget.
pub async fn post(url: &str, payload: &TickWebhookPayload) {
    if url.is_empty() {
        return;
    }
    let body = match serde_json::to_string(payload) {
        Ok(b) => b,
        Err(e) => {
            warn!("webhook: encode failed: {e}");
            return;
        }
    };
    let (host, port, path) = match parse_url(url) {
        Some(t) => t,
        None => {
            warn!("webhook: unsupported URL {url}");
            return;
        }
    };
    let addr = format!("{host}:{port}");
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: cmmd/{ver}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        ver = env!("CARGO_PKG_VERSION"),
        len = body.len(),
        body = body,
    );

    let send = async move {
        let mut sock = TcpStream::connect(&addr).await?;
        sock.write_all(req.as_bytes()).await?;
        sock.shutdown().await.ok();
        // Drain the response (don't actually parse it — best effort).
        let mut sink = [0u8; 1024];
        let _ = sock.read(&mut sink).await;
        Ok::<(), anyhow::Error>(())
    };

    match timeout(Duration::from_secs(5), send).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("webhook: send failed: {e}"),
        Err(_) => warn!("webhook: timed out after 5s"),
    }
}

/// Tiny HTTP URL parser. Returns (host, port, path) for `http://host[:port][/path]`.
/// HTTPS is intentionally rejected — we have no TLS dependency.
fn parse_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.find(':') {
        Some(idx) => (&hostport[..idx], hostport[idx + 1..].parse().ok()?),
        None => (hostport, 80u16),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_host_only() {
        assert_eq!(
            parse_url("http://example.com"),
            Some(("example.com".into(), 80, "/".into()))
        );
    }

    #[test]
    fn parse_host_port_path() {
        assert_eq!(
            parse_url("http://1.2.3.4:8080/hook"),
            Some(("1.2.3.4".into(), 8080, "/hook".into()))
        );
    }

    #[test]
    fn rejects_https() {
        assert!(parse_url("https://example.com").is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_url("ftp://x").is_none());
        assert!(parse_url("notaurl").is_none());
    }
}
