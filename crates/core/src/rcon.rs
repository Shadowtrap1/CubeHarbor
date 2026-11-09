//! Minimal async Source RCON client.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const AUTH: i32 = 3;
const COMMAND: i32 = 2;

pub struct Rcon {
    stream: TcpStream,
    next_id: i32,
    read_timeout: Duration,
}

impl Rcon {
    pub async fn connect(addr: &str, password: &str) -> Result<Self> {
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .context("RCON connect timed out")??;
        let mut r = Self { stream, next_id: 1, read_timeout: Duration::from_secs(10) };
        let id = r.send(AUTH, password).await?;
        // Some servers send an empty RESPONSE_VALUE before the auth response.
        loop {
            let (rid, kind, _) = r.recv().await?;
            if rid == -1 {
                bail!("RCON authentication failed");
            }
            if rid == id && kind == 2 {
                return Ok(r);
            }
        }
    }

    /// How long to wait for a reply. Commands like `save-all flush` can take minutes.
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    pub async fn command(&mut self, cmd: &str) -> Result<String> {
        let id = self.send(COMMAND, cmd).await?;
        // Follow with an empty packet: its echo marks the end of a fragmented response.
        let end = self.send(COMMAND, "").await?;
        let mut out = String::new();
        loop {
            let (rid, _, body) = self.recv().await?;
            if rid == end {
                break;
            }
            if rid == id {
                out.push_str(&body);
            }
        }
        Ok(out)
    }

    async fn send(&mut self, kind: i32, body: &str) -> Result<i32> {
        let id = self.next_id;
        self.next_id += 1;
        let mut pkt = Vec::with_capacity(14 + body.len());
        pkt.extend_from_slice(&((10 + body.len()) as i32).to_le_bytes());
        pkt.extend_from_slice(&id.to_le_bytes());
        pkt.extend_from_slice(&kind.to_le_bytes());
        pkt.extend_from_slice(body.as_bytes());
        pkt.extend_from_slice(&[0, 0]);
        self.stream.write_all(&pkt).await?;
        Ok(id)
    }

    async fn recv(&mut self) -> Result<(i32, i32, String)> {
        let timeout = self.read_timeout;
        let fut = async {
            let len = self.stream.read_i32_le().await?;
            if !(10..=1 << 20).contains(&len) {
                bail!("bad RCON packet length {len}");
            }
            let mut buf = vec![0; len as usize];
            self.stream.read_exact(&mut buf).await?;
            let id = i32::from_le_bytes(buf[0..4].try_into()?);
            let kind = i32::from_le_bytes(buf[4..8].try_into()?);
            let body = String::from_utf8_lossy(&buf[8..buf.len() - 2]).into_owned();
            Ok((id, kind, body))
        };
        tokio::time::timeout(timeout, fut).await.context("RCON read timed out")?
    }
}

/// Strip Minecraft `§x` formatting codes.
pub fn strip_formatting(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}
