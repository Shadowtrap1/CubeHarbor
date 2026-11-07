//! Plain L4 forwarding for allocated public ports (voice chat, Bedrock, extras).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cubeharbor_core::crd::Protocol;
use kube::ResourceExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

use crate::State;

/// UDP sessions without traffic for this long are dropped.
const UDP_IDLE: Duration = Duration::from_secs(180);

#[derive(Clone, PartialEq, Eq, Debug)]
struct Target {
    backend: String,
    owner: String,
}

/// Reconcile listeners with allocations every few seconds.
pub async fn run(state: Arc<State>) {
    let mut active: HashMap<(u16, Protocol), (Target, JoinHandle<()>)> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tick.tick().await;
        let mut desired: HashMap<(u16, Protocol), Target> = HashMap::new();
        for s in state.servers.state() {
            let Some(st) = &s.status else { continue };
            let Some(svc) = &st.service else { continue };
            for a in &st.allocations {
                desired.insert(
                    (a.public_port, a.protocol),
                    Target {
                        backend: format!("{svc}:{}", a.container_port),
                        owner: format!("{}/{} {}", s.namespace().unwrap_or_default(), s.name_any(), a.name),
                    },
                );
            }
        }
        active.retain(|key, (target, handle)| {
            let keep = desired.get(key) == Some(target) && !handle.is_finished();
            if !keep {
                handle.abort();
            }
            keep
        });
        for (key, target) in desired {
            if active.contains_key(&key) {
                continue;
            }
            let (port, proto) = key;
            let bind = join_host_port(&state.cfg.ports_bind, port);
            tracing::info!("forwarding {bind}/{} -> {} ({})", proto.as_str(), target.backend, target.owner);
            let t = target.clone();
            let handle = match proto {
                Protocol::Tcp => tokio::spawn(tcp(bind, t, state.clone())),
                Protocol::Udp => tokio::spawn(udp(bind, t, state.cfg.max_udp_sessions.max(1))),
            };
            active.insert(key, (target, handle));
        }
        state.stats.forwarded_ports.store(active.len() as u64, Ordering::Relaxed);
    }
}

async fn tcp(bind: String, target: Target, state: Arc<State>) {
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot bind {bind}: {e}");
            tokio::time::sleep(Duration::from_secs(10)).await;
            return;
        }
    };
    while let Ok((mut client, peer)) = listener.accept().await {
        // Its own budget: a flood here can't exhaust sockets, nor take slots from the Minecraft port.
        let Ok(permit) = state.forward_connections.clone().try_acquire_owned() else {
            crate::inc(&state.stats.forward_rejected);
            tracing::debug!("{peer} -> {bind}: connection limit reached, dropping");
            continue;
        };
        let backend = target.backend.clone();
        let idle = state.idle_timeout();
        tokio::spawn(async move {
            let _permit = permit;
            if let Ok(Ok(mut up)) = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&backend)).await
                && let Err(e) = crate::pipe::relay(&mut client, &mut up, idle).await
            {
                tracing::debug!("{peer} -> {backend}: {e}");
            }
        });
    }
}

struct Session {
    upstream: Arc<UdpSocket>,
    last_seen: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

async fn udp(bind: String, target: Target, max_sessions: usize) {
    let sock = match UdpSocket::bind(&bind).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("cannot bind {bind}: {e}");
            tokio::time::sleep(Duration::from_secs(10)).await;
            return;
        }
    };
    let start = Instant::now();
    let mut sessions: HashMap<SocketAddr, Session> = HashMap::new();
    let mut buf = vec![0u8; 65535];
    let mut sweep = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let Ok((n, client)) = r else { continue };
                if !sessions.get(&client).is_some_and(|s| !s.task.is_finished()) {
                    // Every new source address costs a socket and a task; spoofed floods must not exhaust them.
                    if sessions.len() >= max_sessions {
                        sessions.retain(|_, s| !s.task.is_finished());
                        if sessions.len() >= max_sessions {
                            continue;
                        }
                    }
                    match new_session(&sock, client, &target.backend, start).await {
                        Ok(s) => { sessions.insert(client, s); }
                        Err(e) => { tracing::debug!("udp session to {}: {e}", target.backend); continue; }
                    }
                }
                let s = &sessions[&client];
                s.last_seen.store(start.elapsed().as_secs(), Ordering::Relaxed);
                let _ = s.upstream.send(&buf[..n]).await;
            }
            _ = sweep.tick() => sessions.retain(|_, s| !s.task.is_finished()),
        }
    }
}

async fn new_session(sock: &Arc<UdpSocket>, client: SocketAddr, backend: &str, start: Instant) -> std::io::Result<Session> {
    let addr = tokio::net::lookup_host(backend)
        .await?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "backend did not resolve"))?;
    let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let upstream = Arc::new(UdpSocket::bind(bind).await?);
    upstream.connect(addr).await?;
    let last_seen = Arc::new(AtomicU64::new(start.elapsed().as_secs()));
    let (up, down, seen) = (upstream.clone(), sock.clone(), last_seen.clone());
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            match tokio::time::timeout(Duration::from_secs(30), up.recv(&mut buf)).await {
                Ok(Ok(n)) => {
                    seen.store(start.elapsed().as_secs(), Ordering::Relaxed);
                    let _ = down.send_to(&buf[..n], client).await;
                }
                Ok(Err(_)) => break,
                Err(_) => {
                    if start.elapsed().as_secs().saturating_sub(seen.load(Ordering::Relaxed)) > UDP_IDLE.as_secs() {
                        break;
                    }
                }
            }
        }
    });
    Ok(Session { upstream, last_seen, task })
}

/// `0.0.0.0` + 25600 -> `0.0.0.0:25600`, `::` + 25600 -> `[::]:25600`.
fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

#[cfg(test)]
mod bind_tests {
    #[test]
    fn ipv4_and_ipv6_binds() {
        assert_eq!(super::join_host_port("0.0.0.0", 25600), "0.0.0.0:25600");
        assert_eq!(super::join_host_port("::", 25600), "[::]:25600");
        assert_eq!(super::join_host_port("[::]", 25600), "[::]:25600");
    }
}
