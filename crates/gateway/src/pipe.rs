//! Bidirectional TCP relaying that gives up on connections idle in both directions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

/// Copy between `a` and `b` until both sides close. With `idle`, the connection is dropped once
/// no bytes have moved either way for that long, so stalled or abandoned clients can't hold a
/// connection slot forever. (Minecraft sends keep-alives every few seconds, so a live game is
/// never idle for minutes.)
pub async fn relay(a: &mut TcpStream, b: &mut TcpStream, idle: Option<Duration>) -> std::io::Result<()> {
    let start = Instant::now();
    let last = AtomicU64::new(0);
    let (ar, aw) = a.split();
    let (br, bw) = b.split();
    let both = async { tokio::try_join!(pump(ar, bw, start, &last), pump(br, aw, start, &last)).map(|_| ()) };
    let Some(idle) = idle else { return both.await };
    let watchdog = async {
        loop {
            let since = Duration::from_millis(last.load(Ordering::Relaxed));
            let deadline = start + since + idle;
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("idle for {}s", idle.as_secs())));
            }
            tokio::time::sleep_until(deadline).await;
        }
    };
    tokio::select! {
        r = both => r,
        e = watchdog => e,
    }
}

async fn pump<R, W>(mut r: R, mut w: W, start: Instant, last: &AtomicU64) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // 8 KiB like `copy_bidirectional`: two per connection, times thousands of connections.
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            // Pass the half-close on, like `copy_bidirectional`.
            return w.shutdown().await;
        }
        w.write_all(&buf[..n]).await?;
        last.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A connected pair (client side, server side).
    async fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (s, _) = l.accept().await.unwrap();
        (c, s)
    }

    #[tokio::test]
    async fn relays_both_ways_and_closes() {
        let (mut client, mut gw_in) = pair().await;
        let (mut gw_out, mut backend) = pair().await;
        let relay = tokio::spawn(async move { relay(&mut gw_in, &mut gw_out, Some(Duration::from_secs(5))).await });
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        backend.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        backend.write_all(b"pong").await.unwrap();
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
        drop(client);
        drop(backend);
        relay.await.unwrap().unwrap();
    }

    /// A client that sends its request and half-closes still gets the backend's later reply.
    #[tokio::test]
    async fn half_close_is_passed_on() {
        let (mut client, mut gw_in) = pair().await;
        let (mut gw_out, mut backend) = pair().await;
        let relay = tokio::spawn(async move { relay(&mut gw_in, &mut gw_out, Some(Duration::from_secs(5))).await });
        client.write_all(b"req").await.unwrap();
        client.shutdown().await.unwrap();
        let mut got = vec![];
        backend.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"req");
        backend.write_all(b"late reply").await.unwrap();
        drop(backend);
        let mut reply = vec![];
        client.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"late reply");
        relay.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn idle_connections_are_dropped() {
        let ms = Duration::from_millis;
        let (mut client, mut gw_in) = pair().await;
        let (mut gw_out, mut backend) = pair().await;
        let relay = tokio::spawn(async move { relay(&mut gw_in, &mut gw_out, Some(ms(1000))).await });
        // Traffic keeps it alive past the first deadline...
        tokio::time::sleep(ms(600)).await;
        client.write_all(b"x").await.unwrap();
        backend.read_exact(&mut [0u8; 1]).await.unwrap();
        tokio::time::sleep(ms(600)).await;
        assert!(!relay.is_finished(), "dropped despite recent traffic");
        // ...and silence ends it.
        let err = tokio::time::timeout(ms(5000), relay).await.expect("idle connection kept open").unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }
}
