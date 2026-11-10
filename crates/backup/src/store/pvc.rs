//! Backups as files on a PersistentVolumeClaim.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cubeharbor_core::backup::{PvcStorage, check_relative_path};
use serde_json::json;

use super::{Backend, ObjectStore, PodWiring, checked};

const MOUNT: &str = "/backup";

pub struct Pvc {
    claim: String,
    sub_path: Option<String>,
}

impl Pvc {
    pub fn new(cfg: &PvcStorage) -> Result<Self, String> {
        if cfg.claim_name.is_empty() {
            return Err("pvc.claimName is required".into());
        }
        if let Some(p) = &cfg.sub_path {
            check_relative_path(p).map_err(|e| format!("pvc.subPath: {e}"))?;
        }
        Ok(Self {
            claim: cfg.claim_name.clone(),
            sub_path: cfg.sub_path.clone(),
        })
    }
}

impl Backend for Pvc {
    fn wiring(&self) -> PodWiring {
        PodWiring {
            env: vec![],
            volumes: vec![
                json!({ "name": "backup", "persistentVolumeClaim": { "claimName": self.claim } }),
            ],
            mounts: vec![json!({ "name": "backup", "mountPath": MOUNT })],
        }
    }

    fn open(&self) -> Result<Box<dyn ObjectStore>> {
        let mut root = PathBuf::from(MOUNT);
        if let Some(p) = &self.sub_path {
            root.push(p);
        }
        Ok(Box::new(Files { root }))
    }
}

pub struct Files {
    pub root: PathBuf,
}

impl Files {
    fn path(&self, key: &str) -> Result<PathBuf> {
        checked(key)?;
        Ok(self.root.join(key))
    }
}

/// Written beside the target and renamed, so a failed backup never leaves a complete-looking file.
fn write_atomically(path: &Path, data: &mut dyn Read) -> Result<u64> {
    let dir = path.parent().context("object path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("partial");
    let result = (|| -> Result<u64> {
        let mut file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let n = io::copy(data, &mut file)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(n)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

impl ObjectStore for Files {
    fn put(&self, key: &str, data: &mut dyn Read) -> Result<u64> {
        write_atomically(&self.path(key)?, data)
    }

    fn get(&self, key: &str) -> Result<Box<dyn Read + Send>> {
        let path = self.path(key)?;
        Ok(Box::new(
            File::open(&path).with_context(|| format!("opening {}", path.display()))?,
        ))
    }

    fn delete(&self, key: &str) -> Result<()> {
        match fs::remove_file(self.path(key)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Files {
        let root =
            std::env::temp_dir().join(format!("cubeharbor-pvc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        Files { root }
    }

    #[test]
    fn put_get_delete() {
        let s = store("rw");
        assert_eq!(s.put("ns/srv/a.tar.gz", &mut &b"hello"[..]).unwrap(), 5);
        let mut out = String::new();
        s.get("ns/srv/a.tar.gz")
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "hello");
        s.delete("ns/srv/a.tar.gz").unwrap();
        s.delete("ns/srv/a.tar.gz").unwrap();
        assert!(s.get("ns/srv/a.tar.gz").is_err());
    }

    #[test]
    fn failed_read_leaves_nothing() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
        }
        let s = store("broken");
        assert!(s.put("a/b.tar.gz", &mut Broken).is_err());
        assert_eq!(fs::read_dir(s.root.join("a")).unwrap().count(), 0);
    }

    #[test]
    fn keys_cannot_escape() {
        let s = store("escape");
        assert!(s.put("../x", &mut &b"x"[..]).is_err());
        assert!(s.delete("/etc/passwd").is_err());
    }

    #[test]
    fn wiring_mounts_the_claim() {
        let w = Pvc::new(&PvcStorage {
            claim_name: "b".into(),
            sub_path: None,
        })
        .unwrap()
        .wiring();
        assert_eq!(w.volumes[0]["persistentVolumeClaim"]["claimName"], "b");
        assert!(
            Pvc::new(&PvcStorage {
                claim_name: "b".into(),
                sub_path: Some("../x".into())
            })
            .is_err()
        );
    }
}
