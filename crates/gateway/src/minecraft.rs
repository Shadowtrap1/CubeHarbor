//! Hostname routing for Minecraft Java connections.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cubeharbor_core::crd::{MINECRAFT_PORT, MinecraftServer, Phase, claim_order};
use cubeharbor_core::protocol::{self, Handshake};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, Patch, PatchParams};
use kube::ResourceExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{State, inc};

/// Accept and route connections until `stop` turns true (graceful shutdown). Connections already
/// open keep running in their own tasks.
pub async fn listen(state: Arc<State>, mut stop: tokio::sync::watch::Receiver<bool>) -> Result<()> {
    let listener = TcpListener::bind(&state.cfg.listen).await.with_context(|| format!("binding {}", state.cfg.listen))?;
    tracing::info!("routing Minecraft connections on {}", state.cfg.listen);
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = stop.wait_for(|s| *s) => {
                tracing::info!("no longer accepting Minecraft connections");
                return Ok(());
            }
        };
        let (stream, peer) = match accepted {
            Ok(a) => a,
            // E.g. out of file descriptors: transient, and must not take the whole gateway down.
            Err(e) => {
                tracing::warn!("accepting a connection: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        inc(&state.stats.accepted);
        let Ok(permit) = state.connections.clone().try_acquire_owned() else {
            inc(&state.stats.rejected);
            tracing::debug!("{peer}: connection limit reached, dropping");
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle(state, stream, peer).await {
                tracing::debug!("{peer}: {e:#}");
            }
        });
    }
}

/// Find the server for a hostname: exact match first, then the default server. When several
/// claim it, the oldest wins (the operator reports the others), never whichever the cache lists first.
pub fn route(servers: &[Arc<MinecraftServer>], host: &str) -> Option<Arc<MinecraftServer>> {
    let oldest = |it: &mut dyn Iterator<Item = &Arc<MinecraftServer>>| it.min_by_key(|s| claim_order(s)).cloned();
    oldest(&mut servers.iter().filter(|s| s.status.as_ref().is_some_and(|st| st.hostnames.iter().any(|h| h == host))))
        .or_else(|| oldest(&mut servers.iter().filter(|s| s.spec.routing.default)))
}

async fn handle(state: Arc<State>, mut client: TcpStream, mut peer: SocketAddr) -> Result<()> {
    client.set_nodelay(true)?;
    let mut buf = Vec::with_capacity(512);

    let trusted = state.trusted_proxies.is_empty() || state.trusted_proxies.iter().any(|c| c.contains(peer.ip()));
    if state.cfg.accept_proxy_protocol && trusted {
        peer = tokio::time::timeout(Duration::from_secs(5), read_proxy_header(&mut client, &mut buf))
            .await
            .context("PROXY header timeout")??
            .unwrap_or(peer);
    }

    let hs = tokio::time::timeout(Duration::from_secs(5), read_handshake(&mut client, &mut buf))
        .await
        .context("handshake timeout")??;
    let servers = state.servers.state();
    let Some(server) = route(&servers, &hs.host) else {
        inc(&state.stats.unknown_host);
        tracing::info!("{peer}: no server for host `{}`", hs.host);
        return offline_reply(&mut client, &hs, &buf, "§cUnknown server", &format!("No server is configured for {}", hs.host)).await;
    };
    let name = format!("{}/{}", server.namespace().unwrap_or_default(), server.name_any());

    let phase = server.phase();
    let ready = phase == Phase::Running;
    if !ready {
        return not_running(&state, &server, &mut client, &hs, &buf, phase).await;
    }

    let backend_addr = backend(&server)?;
    let mut upstream = match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&backend_addr)).await {
        Ok(Ok(s)) => s,
        _ => return not_running(&state, &server, &mut client, &hs, &buf, Phase::Starting).await,
    };
    upstream.set_nodelay(true)?;
    if hs.next_state != 1 {
        tracing::info!("{peer} -> {name} ({})", hs.host);
    }
    if server.spec.routing.proxy_protocol {
        let local = client.local_addr()?;
        upstream.write_all(&protocol::proxy_v2_header(peer, local)).await?;
    }
    upstream.write_all(&buf).await?;
    inc(&state.stats.routed);
    crate::pipe::relay(&mut client, &mut upstream, state.idle_timeout()).await?;
    Ok(())
}

fn backend(server: &MinecraftServer) -> Result<String> {
    let svc = server.status.as_ref().and_then(|s| s.service.clone()).context("server has no service yet")?;
    Ok(format!("{svc}:{MINECRAFT_PORT}"))
}

async fn read_handshake(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Handshake> {
    loop {
        if let Some(hs) = protocol::parse_handshake(buf)? {
            return Ok(hs);
        }
        if buf.len() > protocol::MAX_HANDSHAKE {
            bail!("handshake too large");
        }
        let mut chunk = [0u8; 512];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed during handshake");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Parse a PROXY v1/v2 header from an upstream load balancer, leaving any extra bytes in `buf`.
async fn read_proxy_header(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Option<SocketAddr>> {
    const V2_SIG: &[u8] = b"\r\n\r\n\0\r\nQUIT\n";
    let mut chunk = [0u8; 536];
    while buf.len() < 16 {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed before PROXY header");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if buf.starts_with(V2_SIG) {
        let len = u16::from_be_bytes([buf[14], buf[15]]) as usize;
        while buf.len() < 16 + len {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                bail!("truncated PROXY header");
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let fam = buf[13];
        let addr = match fam {
            0x11 if len >= 12 => {
                let ip = std::net::Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]);
                Some(SocketAddr::from((ip, u16::from_be_bytes([buf[24], buf[25]]))))
            }
            0x21 if len >= 36 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(&buf[16..32]);
                Some(SocketAddr::from((std::net::Ipv6Addr::from(o), u16::from_be_bytes([buf[48], buf[49]]))))
            }
            _ => None,
        };
        buf.drain(..16 + len);
        return Ok(addr);
    }
    if buf.starts_with(b"PROXY ") {
        while !buf.windows(2).any(|w| w == b"\r\n") {
            if buf.len() > 108 {
                bail!("bad PROXY v1 header");
            }
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                bail!("truncated PROXY header");
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let end = buf.windows(2).position(|w| w == b"\r\n").unwrap();
        let line = String::from_utf8_lossy(&buf[..end]).to_string();
        buf.drain(..end + 2);
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() >= 6
            && let (Ok(ip), Ok(port)) = (parts[2].parse::<std::net::IpAddr>(), parts[4].parse::<u16>())
        {
            return Ok(Some(SocketAddr::new(ip, port)));
        }
        return Ok(None);
    }
    Ok(None)
}

async fn not_running(
    state: &State,
    server: &MinecraftServer,
    client: &mut TcpStream,
    hs: &Handshake,
    buf: &[u8],
    phase: Phase,
) -> Result<()> {
    inc(&state.stats.offline);
    let sleeping = server.status.as_ref().is_some_and(|s| s.sleeping);
    let wake = sleeping && server.auto_wake();
    let (motd, kick) = match phase {
        Phase::Sleeping if wake => ("§e☾ Sleeping §7— join to wake it up", "The server is waking up! Try again in a minute."),
        Phase::Sleeping => ("§7Sleeping", "The server is asleep."),
        Phase::Stopped => ("§cStopped", "The server is stopped."),
        Phase::Installing => ("§bInstalling mods…", "The server is installing its modpack. Try again in a few minutes."),
        Phase::Error => ("§cServer error", "The server failed to start. Check the dashboard."),
        _ => ("§eStarting…", "The server is starting. Try again in a moment."),
    };
    let key = format!("{}/{}", server.namespace().unwrap_or_default(), server.name_any());
    let recently = {
        let mut w = state.wakes.lock().unwrap();
        let now = std::time::Instant::now();
        w.retain(|_, t| now.duration_since(*t) < Duration::from_secs(300));
        let seen = w.get(&key).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(30));
        if !seen && hs.next_state != 1 && wake {
            w.insert(key, now);
        }
        seen
    };
    if hs.next_state != 1 && wake && !recently {
        tracing::info!("waking {} for a join attempt", server.name_any());
        // Recorded in the wake ConfigMap (the only object the gateway may write); the operator
        // picks it up and starts the server.
        let api: Api<ConfigMap> = Api::namespaced(state.client.clone(), &state.cfg.namespace);
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs().to_string();
        let key = cubeharbor_core::wake::key(&server.namespace().unwrap_or_default(), &server.name_any());
        let patch = serde_json::json!({ "data": { key: ts } });
        if let Err(e) = api.patch(&state.cfg.wake_configmap, &PatchParams::default(), &Patch::Merge(&patch)).await {
            inc(&state.stats.wake_errors);
            tracing::warn!("could not wake {}: {e}", server.name_any());
        } else {
            inc(&state.stats.wakes);
        }
    }
    offline_reply(client, hs, buf, motd, kick).await
}

/// Answer a status ping or kick a login attempt without a backend.
async fn offline_reply(client: &mut TcpStream, hs: &Handshake, buf: &[u8], motd: &str, kick: &str) -> Result<()> {
    if hs.next_state != 1 {
        client.write_all(&protocol::login_disconnect(kick)).await?;
        client.flush().await?;
        return Ok(());
    }
    let json = protocol::status_json(hs.protocol_version, "cubeharbor", motd, 0, 0);
    let mut pending = buf[hs.frame_len..].to_vec();
    // Status requests and pings are tiny; anything bigger is junk or an attempt to tie up memory.
    const MAX_PENDING: usize = 1024;
    let mut chunk = [0u8; 256];
    // Expect: status request (0x00), then ping (0x01 + 8 bytes) which we echo.
    let mut sent_status = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        while let Some((len, n)) = protocol::read_varint(&pending)? {
            let Ok(len) = usize::try_from(len) else { return Ok(()) };
            let total = n + len;
            if pending.len() < total {
                break;
            }
            let packet: Vec<u8> = pending.drain(..total).collect();
            match packet.get(n) {
                Some(0x00) if !sent_status => {
                    client.write_all(&protocol::status_response(&json)).await?;
                    sent_status = true;
                }
                Some(0x01) => {
                    client.write_all(&packet).await?;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        if pending.len() > MAX_PENDING {
            return Ok(());
        }
        let n = tokio::time::timeout_at(deadline, client.read(&mut chunk)).await??;
        if n == 0 {
            return Ok(());
        }
        pending.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubeharbor_core::crd::{MinecraftServerSpec, MinecraftServerStatus};

    fn server(name: &str, hosts: &[&str], default: bool) -> Arc<MinecraftServer> {
        let mut spec = MinecraftServerSpec::default();
        spec.routing.default = default;
        let mut s = MinecraftServer::new(name, spec);
        s.status = Some(MinecraftServerStatus { hostnames: hosts.iter().map(|h| h.to_string()).collect(), ..Default::default() });
        Arc::new(s)
    }

    /// A client pinging a sleeping server gets our MOTD, and login attempts get a kick message.
    #[tokio::test]
    async fn offline_status_and_kick() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![];
                let hs = read_handshake(&mut s, &mut buf).await.unwrap();
                offline_reply(&mut s, &hs, &buf, "zzz sleeping", "waking up").await.unwrap();
            }
        });
        let status = protocol::ping(&addr.to_string(), "atm.mc.test", 25565).await.unwrap();
        assert_eq!(status.version.unwrap().name, "cubeharbor");

        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&protocol::encode_handshake(767, "atm.mc.test", 25565, 2)).await.unwrap();
        let pkt = protocol::read_packet(&mut c, 4096).await.unwrap();
        assert_eq!(pkt[0], 0x00);
        assert!(String::from_utf8_lossy(&pkt).contains("waking up"));
    }

    #[test]
    fn routing() {
        let servers = vec![server("lobby", &["lobby.mc.test"], true), server("atm", &["atm.mc.test", "modded.mc.test"], false)];
        assert_eq!(route(&servers, "modded.mc.test").unwrap().name_any(), "atm");
        assert_eq!(route(&servers, "unknown.mc.test").unwrap().name_any(), "lobby");
        let no_default = vec![server("atm", &["atm.mc.test"], false)];
        assert!(route(&no_default, "x").is_none());
    }
}
