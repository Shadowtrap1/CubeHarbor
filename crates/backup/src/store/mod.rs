//! Storage backends. A backend is one module here with two halves: [`Backend::wiring`], what the
//! Job's pod needs (used by the operator), and [`Backend::open`], the client (used in the Job).
//!
//! To add one: add an optional field to `BackupStorage` in `crates/core/src/backup.rs`, write the
//! module, and add its arm in [`backend`]. Credentials must reach the pod through `secretKeyRef`,
//! so the operator never reads them.

pub mod pvc;
pub mod s3;

use std::io::Read;

use anyhow::Result;
use cubeharbor_core::backup::{BackupStorage, check_relative_path};
use serde_json::Value;

/// Object storage, as the backup Job sees it. Keys are validated relative paths.
pub trait ObjectStore {
    /// Store everything `data` yields under `key`, replacing any previous object. Returns the
    /// byte count. A read error from `data` must leave no complete object behind.
    fn put(&self, key: &str, data: &mut dyn Read) -> Result<u64>;
    fn get(&self, key: &str) -> Result<Box<dyn Read + Send>>;
    /// Deleting an object that doesn't exist is not an error.
    fn delete(&self, key: &str) -> Result<()>;
}

/// What a backend adds to the Job's pod.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PodWiring {
    pub env: Vec<Value>,
    pub volumes: Vec<Value>,
    pub mounts: Vec<Value>,
}

pub trait Backend: Send + Sync {
    fn wiring(&self) -> PodWiring;
    fn open(&self) -> Result<Box<dyn ObjectStore>>;
}

/// The backend `storage` selects, after validating it. Exactly one must be set.
pub fn backend(storage: &BackupStorage) -> Result<Box<dyn Backend>, String> {
    match storage {
        BackupStorage {
            pvc: Some(p),
            s3: None,
        } => Ok(Box::new(pvc::Pvc::new(p)?)),
        BackupStorage {
            pvc: None,
            s3: Some(s),
        } => Ok(Box::new(s3::S3::new(s)?)),
        _ => Err("storage must set exactly one backend (pvc or s3)".into()),
    }
}

/// The checksum object that sits next to an archive.
pub fn checksum_key(key: &str) -> String {
    format!("{key}.sha256")
}

fn checked(key: &str) -> Result<()> {
    check_relative_path(key).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubeharbor_core::backup::{PvcStorage, S3Storage};

    #[test]
    fn exactly_one_backend() {
        assert!(backend(&BackupStorage::default()).is_err());
        let pvc = PvcStorage {
            claim_name: "b".into(),
            sub_path: None,
        };
        assert!(
            backend(&BackupStorage {
                pvc: Some(pvc.clone()),
                s3: None
            })
            .is_ok()
        );
        assert!(
            backend(&BackupStorage {
                pvc: Some(pvc),
                s3: Some(S3Storage::default())
            })
            .is_err()
        );
    }
}
