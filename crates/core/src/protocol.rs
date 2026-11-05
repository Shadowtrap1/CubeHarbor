//! Just enough of the Minecraft Java protocol to route connections and ping servers.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const MAX_HANDSHAKE: usize = 2048;

/// Read a VarInt from the start of `buf`. Returns (value, bytes used) or None when incomplete.
pub fn read_varint(buf: &[u8]) -> Result<Option<(i32, usize)>> {
    let mut value: u32 = 0;
    for i in 0..5 {
        let Some(&b) = buf.get(i) else { return Ok(None) };
        value |= ((b & 0x7f) as u32) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(Some((value as i32, i + 1)));
        }
    }
    bail!("VarInt too long")
}

pub fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        if v & !0x7f == 0 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
}

pub fn write_string(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as i32);
    out.extend_from_slice(s.as_bytes());
}

/// Prefix a packet body (id + payload) with its length.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    write_varint(&mut out, body.len() as i32);
    out.extend_from_slice(body);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub protocol_version: i32,
    /// Server address as sent by the client, with Forge/proxy suffixes stripped and lower-cased.
    pub host: String,
    pub port: u16,
    /// 1 = status, 2 = login, 3 = transfer
    pub next_state: i32,
    /// Length of the handshake frame in the buffer.
    pub frame_len: usize,
}

/// Parse the first packet of a connection. `Ok(None)` means more bytes are needed.
pub fn parse_handshake(buf: &[u8]) -> Result<Option<Handshake>> {
    if buf.first() == Some(&0xFE) {
        bail!("legacy ping");
    }
    let Some((len, n)) = read_varint(buf)? else { return Ok(None) };
    if len <= 0 || len as usize > MAX_HANDSHAKE {
        bail!("bad handshake length {len}");
    }
    let total = n + len as usize;
    if buf.len() < total {
        return Ok(None);
    }
    let mut p = &buf[n..total];
    let (id, k) = read_varint(p)?.context("truncated")?;
    if id != 0 {
        bail!("expected handshake, got packet {id:#x}");
    }
    p = &p[k..];
    let (protocol_version, k) = read_varint(p)?.context("truncated")?;
    p = &p[k..];
    let (slen, k) = read_varint(p)?.context("truncated")?;
    p = &p[k..];
    let slen = slen as usize;
    if slen > 1024 || p.len() < slen + 3 {
        bail!("bad server address");
    }
    let raw = std::str::from_utf8(&p[..slen]).context("address not utf-8")?;
    let port = u16::from_be_bytes([p[slen], p[slen + 1]]);
    p = &p[slen + 2..];
    let (next_state, _) = read_varint(p)?.context("truncated")?;
    // Forge appends "\0FML\0", BungeeCord/TCPShield append "\0ip\0uuid...".
    let host = crate::crd::normalize_host(raw.split('\0').next().unwrap_or(""));
    Ok(Some(Handshake { protocol_version, host, port, next_state, frame_len: total }))
}

pub fn encode_handshake(protocol_version: i32, host: &str, port: u16, next_state: i32) -> Vec<u8> {
    let mut body = vec![0x00];
    write_varint(&mut body, protocol_version);
    write_string(&mut body, host);
    body.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut body, next_state);
    frame(&body)
}

/// Status response packet (state 1, id 0x00).
pub fn status_response(json: &str) -> Vec<u8> {
    let mut body = vec![0x00];
    write_string(&mut body, json);
    frame(&body)
}

/// Login disconnect packet (state 2, id 0x00) with a plain text reason.
pub fn login_disconnect(reason: &str) -> Vec<u8> {
    let mut body = vec![0x00];
    write_string(&mut body, &serde_json::json!({ "text": reason }).to_string());
    frame(&body)
}

/// Build a status JSON document for servers the gateway answers on behalf of.
pub fn status_json(protocol: i32, version_name: &str, motd: &str, online: u32, max: u32) -> String {
    serde_json::json!({
        "version": { "name": version_name, "protocol": protocol },
        "players": { "max": max, "online": online },
        "description": { "text": motd },
    })
    .to_string()
}

/// Read one framed packet (id + payload) from a stream.
pub async fn read_packet(stream: &mut TcpStream, max: usize) -> Result<Vec<u8>> {
    let mut len_buf = Vec::with_capacity(5);
    let len = loop {
        let b = stream.read_u8().await?;
        len_buf.push(b);
        if let Some((len, _)) = read_varint(&len_buf)? {
            break len as usize;
        }
    };
    if len > max {
        bail!("packet too large ({len} bytes)");
    }
    let mut body = vec![0; len];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

#[derive(Deserialize, Debug, Default)]
pub struct Status {
    #[serde(default)]
    pub players: Option<StatusPlayers>,
    #[serde(default)]
    pub version: Option<StatusVersion>,
    /// MOTD: a plain string or a chat component.
    #[serde(default)]
    pub description: Option<serde_json::Value>,
}

impl Status {
    /// The MOTD as plain text (formatting codes kept).
    pub fn motd(&self) -> String {
        fn text(v: &serde_json::Value, out: &mut String) {
            match v {
                serde_json::Value::String(s) => out.push_str(s),
                serde_json::Value::Object(o) => {
                    o.get("text").into_iter().for_each(|t| text(t, out));
                    o.get("extra").into_iter().for_each(|t| text(t, out));
                }
                serde_json::Value::Array(a) => a.iter().for_each(|t| text(t, out)),
                _ => {}
            }
        }
        let mut out = String::new();
        self.description.iter().for_each(|d| text(d, &mut out));
        out
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct StatusPlayers {
    pub online: u32,
    pub max: u32,
    #[serde(default)]
    pub sample: Vec<SamplePlayer>,
}

#[derive(Deserialize, Debug)]
pub struct SamplePlayer {
    pub name: String,
}

#[derive(Deserialize, Debug)]
pub struct StatusVersion {
    pub name: String,
    pub protocol: i32,
}

/// Server List Ping a server and return its status.
pub async fn ping(addr: &str, host: &str, port: u16) -> Result<Status> {
    ping_with_prefix(addr, host, port, &[]).await
}

/// [`ping`], sending `prefix` (e.g. a PROXY protocol header) before the handshake.
pub async fn ping_with_prefix(addr: &str, host: &str, port: u16, prefix: &[u8]) -> Result<Status> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut s = TcpStream::connect(addr).await?;
        let mut out = prefix.to_vec();
        out.extend(encode_handshake(-1, host, port, 1));
        out.extend(frame(&[0x00]));
        s.write_all(&out).await?;
        let body = read_packet(&mut s, 1 << 20).await?;
        let (id, n) = read_varint(&body)?.context("empty packet")?;
        if id != 0 {
            bail!("unexpected status packet {id:#x}");
        }
        let (slen, k) = read_varint(&body[n..])?.context("truncated")?;
        let start = n + k;
        // A hostile server can send a negative or huge length: no arithmetic on it unchecked.
        let end = usize::try_from(slen).ok().and_then(|l| start.checked_add(l)).context("bad status length")?;
        let json = body.get(start..end).context("truncated status")?;
        Ok(serde_json::from_slice(json)?)
    })
    .await
    .context("ping timed out")?
}

/// PROXY protocol v2 header for a TCP connection.
pub fn proxy_v2_header(src: std::net::SocketAddr, dst: std::net::SocketAddr) -> Vec<u8> {
    use std::net::SocketAddr::*;
    let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    h.push(0x21); // v2, PROXY
    match (src, dst) {
        (V4(s), V4(d)) => {
            h.push(0x11); // TCP over IPv4
            h.extend_from_slice(&12u16.to_be_bytes());
            h.extend_from_slice(&s.ip().octets());
            h.extend_from_slice(&d.ip().octets());
            h.extend_from_slice(&s.port().to_be_bytes());
            h.extend_from_slice(&d.port().to_be_bytes());
        }
        (s, d) => {
            let v6 = |a: std::net::SocketAddr| match a.ip() {
                std::net::IpAddr::V4(ip) => ip.to_ipv6_mapped(),
                std::net::IpAddr::V6(ip) => ip,
            };
            h.push(0x21); // TCP over IPv6
            h.extend_from_slice(&36u16.to_be_bytes());
            h.extend_from_slice(&v6(s).octets());
            h.extend_from_slice(&v6(d).octets());
            h.extend_from_slice(&s.port().to_be_bytes());
            h.extend_from_slice(&d.port().to_be_bytes());
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server answering a ping with a negative string length gets an error, not a panic.
    #[tokio::test]
    async fn ping_rejects_negative_status_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let _ = s.read(&mut [0u8; 512]).await;
            let mut body = vec![0x00];
            write_varint(&mut body, -5);
            s.write_all(&frame(&body)).await.unwrap();
        });
        assert!(ping(&addr.to_string(), "x", 25565).await.is_err());
    }

    /// A ping can lead with a PROXY v2 header, for servers that drop connections without one.
    #[tokio::test]
    async fn ping_with_proxy_header() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let n = s.read(&mut buf).await.unwrap();
            // Like a proxy-protocol server: no header, no answer.
            if !buf[..n].starts_with(b"\r\n\r\n\0\r\nQUIT\n") {
                return;
            }
            s.write_all(&status_response(&status_json(767, "test", "hi", 0, 1))).await.unwrap();
        });
        let header = proxy_v2_header(addr, addr);
        let st = ping_with_prefix(&addr.to_string(), "localhost", addr.port(), &header).await.unwrap();
        assert_eq!(st.version.unwrap().name, "test");
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0, 1, 127, 128, 255, 25565, 2097151, i32::MAX, -1] {
            let mut b = vec![];
            write_varint(&mut b, v);
            assert_eq!(read_varint(&b).unwrap(), Some((v, b.len())));
        }
    }

    #[test]
    fn handshake_parsing() {
        let pkt = encode_handshake(767, "Survival.Example.com.\0FML3\0", 25565, 2);
        let mut buf = pkt.clone();
        buf.extend_from_slice(b"login-bytes");
        let hs = parse_handshake(&buf).unwrap().unwrap();
        assert_eq!(hs.host, "survival.example.com");
        assert_eq!(hs.protocol_version, 767);
        assert_eq!(hs.next_state, 2);
        assert_eq!(hs.frame_len, pkt.len());
        // Incomplete input asks for more.
        assert!(parse_handshake(&pkt[..pkt.len() - 1]).unwrap().is_none());
        assert!(parse_handshake(&[0xFE, 0x01]).is_err());
    }
}
