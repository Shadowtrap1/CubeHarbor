//! What the backup Job does: settings from the environment, RCON save control, upload, verify.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cubeharbor_core::backup::BackupStorage;
use cubeharbor_core::rcon::Rcon;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::archive;
use crate::store::{self, ObjectStore, checksum_key};

/// `save-all flush` can take minutes on a big world.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest error message kept: the kubelet caps a termination message at 4 KiB.
const MAX_MESSAGE: usize = 2000;

/// Environment variable names, shared with the operator's Job builder.
pub mod env {
    pub const KEY: &str = "BACKUP_KEY";
    pub const STORAGE: &str = "BACKUP_STORAGE";
    pub const EXCLUDE: &str = "BACKUP_EXCLUDE";
    pub const DELETE_KEYS: &str = "BACKUP_DELETE_KEYS";
    pub const RCON_ADDR: &str = "RCON_ADDR";
    pub const RCON_PASSWORD: &str = "RCON_PASSWORD";
    pub const DATA_DIR: &str = "CUBEHARBOR_DATA";
}

pub struct Settings {
    pub data_dir: PathBuf,
    pub key: String,
    pub storage: Option<BackupStorage>,
    pub exclude: Vec<String>,
    pub delete_keys: Vec<String>,
    /// Address and password. Unset for a server that is stopped.
    pub rcon: Option<(String, String)>,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let json_list = |name: &str| -> Result<Vec<String>> {
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => {
                    serde_json::from_str(&v).with_context(|| format!("parsing {name}"))
                }
                _ => Ok(vec![]),
            }
        };
        let storage = std::env::var(env::STORAGE)
            .ok()
            .map(|s| serde_json::from_str(&s).context("parsing BACKUP_STORAGE"))
            .transpose()?;
        let rcon = match (
            std::env::var(env::RCON_ADDR),
            std::env::var(env::RCON_PASSWORD),
        ) {
            (Ok(a), Ok(p)) if !a.is_empty() => Some((a, p)),
            _ => None,
        };
        Ok(Self {
            data_dir: std::env::var(env::DATA_DIR)
                .unwrap_or_else(|_| "/data".into())
                .into(),
            key: std::env::var(env::KEY).unwrap_or_default(),
            storage,
            exclude: json_list(env::EXCLUDE)?,
            delete_keys: json_list(env::DELETE_KEYS)?,
            rcon,
        })
    }

    fn open(&self) -> Result<Box<dyn ObjectStore>> {
        let storage = self.storage.as_ref().context("BACKUP_STORAGE is not set")?;
        store::backend(storage).map_err(anyhow::Error::msg)?.open()
    }
}

/// What the operator reads from the pod's termination message.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Saves are back on (or were never turned off), so the operator need not clean up.
    #[serde(default)]
    pub saves_restored: bool,
}

impl Report {
    pub fn failure(error: String) -> Self {
        let error = error.chars().take(MAX_MESSAGE).collect();
        Self {
            error: Some(error),
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

pub fn write_report(path: &Path, report: &Report) {
    if let Err(e) = std::fs::write(path, report.to_json()) {
        eprintln!("writing {}: {e}", path.display());
    }
}

fn block_on<T>(fut: impl Future<Output = T>) -> Result<T> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fut))
}

async fn rcon_commands(addr: &str, password: &str, commands: &[&str]) -> Result<()> {
    let mut rcon = Rcon::connect(addr, password).await?;
    rcon.set_read_timeout(FLUSH_TIMEOUT);
    for c in commands {
        let reply = rcon.command(c).await.with_context(|| format!("RCON {c}"))?;
        println!("rcon {c}: {}", reply.trim());
    }
    Ok(())
}

/// `save-on`, retried: leaving autosave off silently loses progress on the next crash.
fn resume_saves(addr: &str, password: &str) -> Result<()> {
    let mut last = None;
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(5));
        }
        match block_on(rcon_commands(addr, password, &["save-on"]))? {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("at least one attempt"))
}

/// Turn saves back on if the Job is terminated (deadline, drain, delete).
fn resume_saves_on_sigterm(addr: String, password: String) {
    std::thread::spawn(move || {
        let waited = block_on(async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut s) => s.recv().await.is_some(),
                Err(_) => false,
            }
        });
        if matches!(waited, Ok(true)) {
            let restored = resume_saves(&addr, &password).is_ok();
            let report = Report {
                saves_restored: restored,
                ..Report::failure("terminated".into())
            };
            write_report(Path::new("/dev/termination-log"), &report);
            std::process::exit(143);
        }
    });
}

enum Chunk {
    Data(Vec<u8>),
    Failed(String),
    Done,
}

/// Bounded pipe from the archiver thread to the uploader, so memory stays flat.
struct PipeWriter(SyncSender<Chunk>);
struct PipeReader {
    rx: Receiver<Chunk>,
    current: Vec<u8>,
    pos: usize,
    done: bool,
}

fn pipe() -> (PipeWriter, PipeReader) {
    let (tx, rx) = sync_channel(16);
    (
        PipeWriter(tx),
        PipeReader {
            rx,
            current: vec![],
            pos: 0,
            done: false,
        },
    )
}

impl Write for &PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .send(Chunk::Data(buf.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the upload stopped"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.current.len() {
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Chunk::Data(d)) => (self.current, self.pos) = (d, 0),
                Ok(Chunk::Done) => self.done = true,
                Ok(Chunk::Failed(e)) => return Err(io::Error::other(e)),
                // The archiver died without saying it finished: never treat that as a clean end.
                Err(_) => return Err(io::Error::other("the archiver stopped unexpectedly")),
            }
        }
        let n = buf.len().min(self.current.len() - self.pos);
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct Hashing<R> {
    inner: R,
    hash: Sha256,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Archive `data_dir` into the store. Returns size and SHA-256.
pub fn upload(
    store: &dyn ObjectStore,
    key: &str,
    data_dir: &Path,
    exclude: &[String],
) -> Result<(u64, String)> {
    let (writer, reader) = pipe();
    let mut reader = Hashing {
        inner: reader,
        hash: Sha256::new(),
    };
    let size = std::thread::scope(|s| {
        s.spawn(|| {
            let end = match archive::create(
                data_dir,
                exclude,
                io::BufWriter::with_capacity(1 << 20, &writer),
            ) {
                Ok(()) => Chunk::Done,
                Err(e) => Chunk::Failed(format!("{e:#}")),
            };
            let _ = writer.0.send(end);
        });
        let result = store.put(key, &mut reader);
        // Unblocks the archiver if the upload stopped early.
        drop(std::mem::replace(&mut reader.inner, pipe().1));
        result
    })?;
    let sha = hex(&reader.hash.finalize());
    store.put(&checksum_key(key), &mut sha.as_bytes())?;
    Ok((size, sha))
}

pub fn backup(s: &Settings) -> Result<Report> {
    let store = s.open()?;
    let mut saves_restored = true;
    let result = match &s.rcon {
        None => upload(store.as_ref(), &s.key, &s.data_dir, &s.exclude),
        Some((addr, password)) => {
            resume_saves_on_sigterm(addr.clone(), password.clone());
            // Saves first go off, then one full flush, so the files are stable while archived.
            match block_on(rcon_commands(
                addr,
                password,
                &["save-off", "save-all flush"],
            ))? {
                Err(e) => {
                    saves_restored = resume_saves(addr, password).is_ok();
                    Err(e.context("pausing saves over RCON"))
                }
                Ok(()) => {
                    let result = upload(store.as_ref(), &s.key, &s.data_dir, &s.exclude);
                    match resume_saves(addr, password) {
                        Ok(()) => result,
                        Err(e) => {
                            saves_restored = false;
                            result.and(Err(e.context("turning saves back on")))
                        }
                    }
                }
            }
        }
    };
    let (size, sha) = match result {
        Ok(v) => v,
        // A report, not an Err: the operator needs to know whether saves came back on.
        Err(e) => {
            return Ok(Report {
                saves_restored,
                ..Report::failure(format!("{e:#}"))
            });
        }
    };
    // Retention: only after this backup is safely stored.
    for key in &s.delete_keys {
        for k in [key.clone(), checksum_key(key)] {
            if let Err(e) = store.delete(&k) {
                eprintln!("deleting expired backup {k}: {e:#}");
            }
        }
    }
    Ok(Report {
        size_bytes: Some(size),
        sha256: Some(sha),
        error: None,
        saves_restored,
    })
}

/// Download, verify and unpack into a staging directory, then swap it in. Until the swap the
/// server's current data is untouched.
pub fn restore(s: &Settings) -> Result<Report> {
    let store = s.open()?;
    restore_from(store.as_ref(), &s.key, &s.data_dir)
}

pub fn restore_from(store: &dyn ObjectStore, key: &str, data_dir: &Path) -> Result<Report> {
    let staging = data_dir.join(archive::STAGING_DIR);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let mut input = Hashing {
        inner: store
            .get(key)
            .with_context(|| format!("downloading {key}"))?,
        hash: Sha256::new(),
    };
    let extracted = archive::extract(&mut input, &staging);
    let check = extracted.and_then(|()| {
        let actual = hex(&input.hash.clone().finalize());
        let expected = store.get(&checksum_key(key)).and_then(|mut r| {
            let mut s = String::new();
            r.read_to_string(&mut s)?;
            Ok(s.trim().to_string())
        });
        match expected {
            Ok(e) if e != actual => {
                bail!("checksum mismatch for {key}: stored {e}, computed {actual}")
            }
            Ok(_) => Ok(actual),
            // An archive brought in by hand may have no checksum; a good gzip stream still verified it.
            Err(_) => Ok(actual),
        }
    });
    let sha = match check {
        Ok(sha) => sha,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    archive::swap_in_staged(data_dir)?;
    Ok(Report {
        sha256: Some(sha),
        saves_restored: true,
        ..Default::default()
    })
}

/// Cleanup after a backup Job died without turning saves back on.
pub fn save_on(s: &Settings) -> Result<Report> {
    let (addr, password) = s.rcon.as_ref().context("RCON_ADDR is not set")?;
    resume_saves(addr, password)?;
    Ok(Report {
        saves_restored: true,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, Vec<u8>>>);

    impl ObjectStore for Memory {
        fn put(&self, key: &str, data: &mut dyn Read) -> Result<u64> {
            let mut v = vec![];
            data.read_to_end(&mut v)?;
            let n = v.len() as u64;
            self.0.lock().unwrap().insert(key.into(), v);
            Ok(n)
        }

        fn get(&self, key: &str) -> Result<Box<dyn Read + Send>> {
            let v = self
                .0
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .context("missing")?;
            Ok(Box::new(io::Cursor::new(v)))
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cubeharbor-run-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn backup_then_restore_replaces_the_data() {
        let data = scratch("rt");
        fs::create_dir_all(data.join("world")).unwrap();
        fs::write(data.join("world/level.dat"), b"v1").unwrap();
        let store = Memory::default();
        let (size, sha) = upload(&store, "k.tar.gz", &data, &[]).unwrap();
        assert!(size > 0);
        assert_eq!(
            String::from_utf8(store.0.lock().unwrap()["k.tar.gz.sha256"].clone()).unwrap(),
            sha
        );

        fs::write(data.join("world/level.dat"), b"v2").unwrap();
        fs::write(data.join("griefed"), b"x").unwrap();
        restore_from(&store, "k.tar.gz", &data).unwrap();
        assert_eq!(fs::read(data.join("world/level.dat")).unwrap(), b"v1");
        assert!(!data.join("griefed").exists() && !data.join(archive::STAGING_DIR).exists());
    }

    #[test]
    fn corrupt_archive_leaves_the_data_alone() {
        let data = scratch("corrupt");
        fs::write(data.join("level.dat"), b"v1").unwrap();
        let store = Memory::default();
        upload(&store, "k", &data, &[]).unwrap();
        fs::write(data.join("level.dat"), b"current").unwrap();

        // A valid archive under the wrong checksum.
        store.put("k.sha256", &mut &b"0000"[..]).unwrap();
        assert!(
            restore_from(&store, "k", &data)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
        // Bytes that aren't an archive.
        store.put("k", &mut &b"garbage"[..]).unwrap();
        assert!(restore_from(&store, "k", &data).is_err());

        assert_eq!(fs::read(data.join("level.dat")).unwrap(), b"current");
        assert!(!data.join(archive::STAGING_DIR).exists());
    }

    #[test]
    fn missing_source_directory_fails_the_upload() {
        let missing = scratch("gone").join("nope");
        let store = Memory::default();
        assert!(upload(&store, "k", &missing, &[]).is_err());
    }

    #[test]
    fn archiver_failure_is_a_read_error_not_end_of_file() {
        let (w, mut r) = pipe();
        w.0.send(Chunk::Failed("disk".into())).unwrap();
        assert!(r.read(&mut [0; 4]).is_err());
        let (w, mut r) = pipe();
        drop(w);
        assert!(
            r.read(&mut [0; 4]).is_err(),
            "a vanished writer must not look like a clean end"
        );
    }

    #[test]
    fn report_round_trips_and_is_bounded() {
        let r = Report::failure("x".repeat(100_000));
        assert!(r.to_json().len() < 4096);
        let parsed: Report = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(parsed, r);
    }
}
