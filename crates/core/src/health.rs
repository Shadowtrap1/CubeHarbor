//! A tiny HTTP endpoint for probes and Prometheus scrapes (`/healthz`, `/readyz`, `/metrics`).
//!
//! It answers one `GET` per connection and closes it, which is all kubelet probes and Prometheus
//! need, without pulling an HTTP stack into the gateway and operator.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Response {
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Response { status, content_type: "text/plain; charset=utf-8", body: body.into() }
    }

    /// Prometheus text exposition format 0.0.4.
    pub fn metrics(body: String) -> Self {
        Response { status: 200, content_type: "text/plain; version=0.0.4; charset=utf-8", body }
    }

    pub fn not_found() -> Self {
        Self::text(404, "not found\n")
    }
}

/// Answer `GET <path>` with `handler(path)` (the query string is stripped) until the listener
/// fails to bind. Accept errors (e.g. out of file descriptors) are logged and retried.
pub async fn serve<F>(addr: &str, handler: F) -> std::io::Result<()>
where
    F: Fn(&str) -> Response + Send + Sync + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    let handler = Arc::new(handler);
    loop {
        let mut s = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                tracing::warn!("health endpoint accept: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let Ok(Some(path)) = tokio::time::timeout(Duration::from_secs(5), read_path(&mut s)).await else {
                return;
            };
            let r = handler(&path);
            let _ = s.write_all(&encode(&r)).await;
            let _ = s.shutdown().await;
        });
    }
}

/// Spawn [`serve`], logging (not exiting) if the address can't be bound.
pub fn spawn<F>(addr: String, handler: F) -> tokio::task::JoinHandle<()>
where
    F: Fn(&str) -> Response + Send + Sync + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = serve(&addr, handler).await {
            tracing::error!("health endpoint {addr}: {e}");
        }
    })
}

/// Reads the request head and returns the path of a `GET`/`HEAD` request.
async fn read_path<S: AsyncReadExt + Unpin>(s: &mut S) -> Option<String> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && !buf.windows(2).any(|w| w == b"\n\n") {
        if buf.len() > 8 * 1024 {
            return None;
        }
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    parse_request_line(&buf)
}

fn parse_request_line(buf: &[u8]) -> Option<String> {
    let line = buf.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" && method != "HEAD" {
        return Some(String::new());
    }
    Some(target.split('?').next().unwrap_or("").to_string())
}

fn encode(r: &Response) -> Vec<u8> {
    let reason = match r.status {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "",
    };
    let mut out = format!(
        "HTTP/1.1 {} {reason}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        r.status,
        r.content_type,
        r.body.len()
    )
    .into_bytes();
    out.extend_from_slice(r.body.as_bytes());
    out
}

/// Builds a Prometheus text exposition. Each metric family is written once with its HELP/TYPE.
#[derive(Default)]
pub struct Metrics(String);

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    fn header(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    pub fn counter(&mut self, name: &str, help: &str, value: u64) -> &mut Self {
        self.header(name, "counter", help);
        let _ = writeln!(self.0, "{name} {value}");
        self
    }

    pub fn gauge(&mut self, name: &str, help: &str, value: f64) -> &mut Self {
        self.header(name, "gauge", help);
        let _ = writeln!(self.0, "{name} {value}");
        self
    }

    /// A summary without quantiles: `<name>_sum` and `<name>_count`.
    pub fn summary(&mut self, name: &str, help: &str, sum: f64, count: u64) -> &mut Self {
        self.header(name, "summary", help);
        let _ = writeln!(self.0, "{name}_sum {sum}\n{name}_count {count}");
        self
    }

    /// One family with a sample per label set, e.g. `("phase", "Running")`.
    pub fn labeled<'a>(&mut self, name: &str, kind: &str, help: &str, samples: impl IntoIterator<Item = (&'a [(&'a str, &'a str)], f64)>) -> &mut Self {
        self.header(name, kind, help);
        for (labels, value) in samples {
            let labels: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", escape(v))).collect();
            let _ = writeln!(self.0, "{name}{{{}}} {value}", labels.join(","));
        }
        self
    }

    pub fn finish(&mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Resolves on SIGTERM or SIGINT (Ctrl-C).
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
        (Ok(mut term), Ok(mut int)) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        // Without signal handlers, never "receive" one: the process is then stopped by SIGKILL.
        _ => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines() {
        assert_eq!(parse_request_line(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").as_deref(), Some("/healthz"));
        assert_eq!(parse_request_line(b"GET /metrics?x=1 HTTP/1.1\r\n\r\n").as_deref(), Some("/metrics"));
        assert_eq!(parse_request_line(b"POST /healthz HTTP/1.1\r\n\r\n").as_deref(), Some(""));
        assert_eq!(parse_request_line(b"garbage").as_deref(), None);
    }

    #[test]
    fn exposition() {
        let mut m = Metrics::new();
        m.counter("x_total", "Things.", 3).gauge("y", "Level.", 1.5).labeled(
            "z",
            "gauge",
            "By phase.",
            [(&[("phase", "Running")][..], 2.0), (&[("phase", "a\"b")][..], 0.0)],
        );
        let out = m.finish();
        assert_eq!(
            out,
            "# HELP x_total Things.\n# TYPE x_total counter\nx_total 3\n\
             # HELP y Level.\n# TYPE y gauge\ny 1.5\n\
             # HELP z By phase.\n# TYPE z gauge\nz{phase=\"Running\"} 2\nz{phase=\"a\\\"b\"} 0\n"
        );
    }

    #[tokio::test]
    async fn serves_paths() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        spawn(addr.clone(), |p| if p == "/healthz" { Response::text(200, "ok") } else { Response::not_found() });
        let get = |path: &'static str| {
            let addr = addr.clone();
            async move {
                for _ in 0..50 {
                    if let Ok(mut s) = tokio::net::TcpStream::connect(&addr).await {
                        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes()).await.unwrap();
                        let mut out = String::new();
                        s.read_to_string(&mut out).await.unwrap();
                        return out;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                panic!("health endpoint never came up");
            }
        };
        let ok = get("/healthz").await;
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n") && ok.ends_with("\r\n\r\nok"), "{ok}");
        assert!(get("/nope").await.starts_with("HTTP/1.1 404"));
    }
}
