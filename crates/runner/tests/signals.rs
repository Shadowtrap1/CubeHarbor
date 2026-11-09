//! Runs the real runner binary against fake `java` scripts and checks shutdown and exit codes.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// One at a time: forking while another test still holds a script open for writing fails with
/// "text file busy".
static SERIAL: Mutex<()> = Mutex::new(());
static NEXT: AtomicUsize = AtomicUsize::new(0);

const LOOP: &str = "while :; do sleep 0.05; done";

struct Server {
    dir: PathBuf,
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Server {
    /// `script` runs as `java`, in the data directory, and must `: > started` once it is set up.
    fn start(script: &str, stop_timeout_secs: u32) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cubeharbor-runner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".cubeharbor")).unwrap();
        std::fs::write(dir.join(".cubeharbor/launch.json"), r#"{"javaArgs":[]}"#).unwrap();
        let java = dir.join("fake-java");
        std::fs::write(&java, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();

        let child = Command::new(env!("CARGO_BIN_EXE_cubeharbor-runner"))
            .env("CUBEHARBOR_DATA", &dir)
            .env("JAVA_BIN", &java)
            .env("MEMORY_LIMIT_MIB", "1024")
            .env("STOP_TIMEOUT_SECONDS", stop_timeout_secs.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self { dir, child }
    }

    fn wait_started(&self) {
        wait_for(
            || self.dir.join("started").exists(),
            "the fake server to start",
        );
    }

    fn signal(&self, sig: libc::c_int) {
        // SAFETY: `kill` has no memory-safety requirements; the child hasn't been waited on.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, sig) },
            0
        );
    }

    fn exit_code(&mut self) -> i32 {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().expect("the runner exits, it isn't killed");
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "runner did not exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn data(&self) -> &Path {
        &self.dir
    }
}

fn wait_for(cond: impl Fn() -> bool, what: &str) {
    let started = Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn exit_code_of_the_server_is_propagated() {
    let _l = lock();
    assert_eq!(Server::start("exit 3", 5).exit_code(), 3);
    assert_eq!(Server::start("exit 0", 5).exit_code(), 0);
}

#[test]
fn death_by_signal_is_reported_as_128_plus_signal() {
    let _l = lock();
    assert_eq!(Server::start("kill -9 $$", 5).exit_code(), 137);
}

#[test]
fn sigterm_becomes_a_console_stop() {
    let _l = lock();
    let mut s = Server::start(
        ": > started\nwhile read -r l; do if [ \"$l\" = stop ]; then : > got-stop; exit 7; fi; done",
        10,
    );
    s.wait_started();
    s.signal(libc::SIGTERM);
    assert_eq!(s.exit_code(), 7);
    assert!(s.data().join("got-stop").exists());
}

#[test]
fn sigint_also_stops_the_server() {
    let _l = lock();
    let mut s = Server::start(
        ": > started\nwhile read -r l; do [ \"$l\" = stop ] && exit 0; done",
        10,
    );
    s.wait_started();
    s.signal(libc::SIGINT);
    assert_eq!(s.exit_code(), 0);
}

#[test]
fn stdin_reaches_the_console() {
    let _l = lock();
    let mut s = Server::start(
        ": > started\nwhile read -r l; do if [ \"$l\" = hello ]; then exit 9; fi; done",
        10,
    );
    s.wait_started();
    s.child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"hello\n")
        .unwrap();
    assert_eq!(s.exit_code(), 9);
}

#[test]
fn a_server_ignoring_stop_gets_sigterm() {
    let _l = lock();
    let mut s = Server::start(&format!("trap 'exit 42' TERM\n: > started\n{LOOP}"), 1);
    s.wait_started();
    s.signal(libc::SIGTERM);
    assert_eq!(s.exit_code(), 42);
}

#[test]
fn a_server_ignoring_everything_is_killed() {
    let _l = lock();
    let mut s = Server::start(&format!("trap '' TERM\n: > started\n{LOOP}"), 1);
    s.wait_started();
    let started = Instant::now();
    s.signal(libc::SIGTERM);
    assert_eq!(s.exit_code(), 137);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

/// The console pipe is full because the server stopped reading: shutdown must still work.
#[test]
fn a_full_console_pipe_does_not_block_shutdown() {
    let _l = lock();
    let mut s = Server::start(&format!("trap 'exit 42' TERM\n: > started\n{LOOP}"), 1);
    s.wait_started();
    let line = vec![b'x'; 1023];
    let mut stdin = s.child.stdin.take().unwrap();
    std::thread::spawn(move || {
        // Far more than the 64 KiB pipe plus the runner's queue. Blocks when everything is full.
        for _ in 0..1024 {
            if stdin
                .write_all(&line)
                .and_then(|()| stdin.write_all(b"\n"))
                .is_err()
            {
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(30));
    });
    std::thread::sleep(Duration::from_millis(300));
    s.signal(libc::SIGTERM);
    assert_eq!(s.exit_code(), 42);
}
