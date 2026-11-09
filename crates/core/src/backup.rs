//! The backup custom resources: `MinecraftBackup` (one backup), `MinecraftBackupSchedule` (cron
//! plus retention) and `MinecraftRestore`.
//!
//! The specs describe intent; the operator runs each one as a Job and records the outcome in the
//! status. Adding a storage backend starts with one more optional field on [`BackupStorage`]; the
//! rest lives in one module of `crates/backup/src/store`.

use std::collections::BTreeMap;

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Names become Job names with a suffix, which must fit a 63-character label.
pub const MAX_BACKUP_NAME: usize = 52;
/// Children are named `<schedule>-<unix time>`, which must fit [`MAX_BACKUP_NAME`].
pub const MAX_SCHEDULE_NAME: usize = 40;

/// On a `MinecraftServer` while a backup or restore owns its data: `<mode>:<kind>/<name>`.
/// With mode [`LOCK_STOP`] the operator keeps the server scaled to zero.
pub const LOCK_ANNOTATION: &str = "cubeharbor.dev/data-lock";
pub const LOCK_STOP: &str = "stop";
pub const LOCK_ONLINE: &str = "online";
/// On a `MinecraftBackup` created by a schedule: the schedule's name.
pub const SCHEDULE_LABEL: &str = "cubeharbor.dev/schedule";

/// True when a backup or restore needs this server stopped.
pub fn stop_requested(annotations: &BTreeMap<String, String>) -> bool {
    annotations
        .get(LOCK_ANNOTATION)
        .is_some_and(|l| l.starts_with("stop:"))
}

/// Where backups are kept. Set exactly one backend.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BackupStorage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc: Option<PvcStorage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Storage>,
}

/// A PersistentVolumeClaim in the server's namespace. Use ReadWriteMany (or a volume on the
/// server's node): the backup of a running server runs next to the server.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PvcStorage {
    pub claim_name: String,
    /// Directory inside the volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
}

/// An S3-compatible bucket (AWS, MinIO, R2, Backblaze B2...).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3Storage {
    pub bucket: String,
    #[serde(default = "default_region")]
    pub region: String,
    /// `https://...` of the service. AWS when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `<endpoint>/<bucket>/<key>` instead of `<bucket>.<endpoint>/<key>`. Defaults to true with
    /// a custom endpoint and false on AWS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_style: Option<bool>,
    /// Key prefix inside the bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// An existing Secret in the server's namespace. Only the backup Job reads it.
    pub credentials_secret: S3CredentialsSecret,
}

fn default_region() -> String {
    "us-east-1".into()
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct S3CredentialsSecret {
    pub name: String,
    /// Key holding the access key ID. Default `accessKeyId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id_key: Option<String>,
    /// Key holding the secret access key. Default `secretAccessKey`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key_key: Option<String>,
    /// Key holding a session token, used when the Secret has it. Default `sessionToken`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token_key: Option<String>,
}

/// One backup of one server. Create it to back up now; deleting it keeps the stored archive.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, Default, KubeSchema, PartialEq)]
#[kube(
    validation = Rule::new("self.metadata.name.size() <= 52 && self.metadata.name.matches('^[a-z]([-a-z0-9]*[a-z0-9])?$')")
        .message("metadata.name must be a DNS-1035 label of at most 52 characters"),
    group = "cubeharbor.dev",
    version = "v1alpha1",
    kind = "MinecraftBackup",
    namespaced,
    status = "MinecraftBackupStatus",
    shortname = "mcb",
    printcolumn = r#"{"name":"Server","type":"string","jsonPath":".spec.server"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".status.mode"}"#,
    printcolumn = r#"{"name":"Size","type":"integer","jsonPath":".status.sizeBytes"}"#,
    printcolumn = r#"{"name":"Completed","type":"date","jsonPath":".status.completedAt"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftBackupSpec {
    /// Name of the `MinecraftServer` in this namespace.
    pub server: String,
    pub storage: BackupStorage,
    /// Paths relative to the server directory to leave out, on top of logs, caches and crash reports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Give up after this long. Default 21600 (6 hours).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, JsonSchema, PartialEq, Eq)]
pub enum JobPhase {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
}

impl JobPhase {
    pub fn finished(self) -> bool {
        matches!(self, JobPhase::Completed | JobPhase::Failed)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftBackupStatus {
    #[serde(default)]
    pub phase: JobPhase,
    /// `online` (saves paused over RCON) or `offline` (server stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Where the archive is, relative to the storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Backs a server up on a cron schedule and prunes old backups.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, Default, KubeSchema, PartialEq)]
#[kube(
    validation = Rule::new("self.metadata.name.size() <= 40 && self.metadata.name.matches('^[a-z]([-a-z0-9]*[a-z0-9])?$')")
        .message("metadata.name must be a DNS-1035 label of at most 40 characters"),
    group = "cubeharbor.dev",
    version = "v1alpha1",
    kind = "MinecraftBackupSchedule",
    namespaced,
    status = "MinecraftBackupScheduleStatus",
    shortname = "mcbs",
    printcolumn = r#"{"name":"Server","type":"string","jsonPath":".spec.server"}"#,
    printcolumn = r#"{"name":"Schedule","type":"string","jsonPath":".spec.schedule"}"#,
    printcolumn = r#"{"name":"Last","type":"date","jsonPath":".status.lastScheduleTime"}"#,
    printcolumn = r#"{"name":"Next","type":"date","jsonPath":".status.nextScheduleTime"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftBackupScheduleSpec {
    pub server: String,
    /// Five-field cron expression in UTC, or `@hourly`, `@daily`, `@weekly`, `@monthly`.
    pub schedule: String,
    pub storage: BackupStorage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Which backups to keep. Everything is kept when empty.
    #[serde(default)]
    pub retention: Retention,
    /// Stop creating backups. Existing ones are kept.
    #[serde(default)]
    pub suspend: bool,
}

/// A backup is removed once it falls outside either limit. Only backups this schedule made are
/// considered, and only after a newer one succeeded.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    /// Keep this many newest successful backups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_last: Option<u32>,
    /// Remove backups older than this many days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_days: Option<u32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftBackupScheduleStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_schedule_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_schedule_time: Option<String>,
    /// The newest backup that succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_backup: Option<String>,
    /// The backup in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_backup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Replaces a server's data with a backup. The server is stopped while it runs.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, Default, KubeSchema, PartialEq)]
#[kube(
    validation = Rule::new("self.metadata.name.size() <= 52 && self.metadata.name.matches('^[a-z]([-a-z0-9]*[a-z0-9])?$')")
        .message("metadata.name must be a DNS-1035 label of at most 52 characters"),
    group = "cubeharbor.dev",
    version = "v1alpha1",
    kind = "MinecraftRestore",
    namespaced,
    status = "MinecraftRestoreStatus",
    shortname = "mcr",
    printcolumn = r#"{"name":"Server","type":"string","jsonPath":".spec.target.server"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Completed","type":"date","jsonPath":".status.completedAt"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftRestoreSpec {
    pub source: RestoreSource,
    pub target: RestoreTarget,
    /// Give up after this long. Default 21600 (6 hours).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

/// Either `backup`, or `storage` and `key` for an archive with no `MinecraftBackup` here.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSource {
    /// Name of a completed `MinecraftBackup` in this namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<BackupStorage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreTarget {
    /// Name of the `MinecraftServer` in this namespace.
    pub server: String,
    /// Must be true to replace the data of a server that already exists.
    #[serde(default)]
    pub overwrite_existing: bool,
    /// A `MinecraftServer` spec, used to create the server when it doesn't exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "free_form")]
    pub new_server: Option<serde_json::Value>,
}

/// Checked against the `MinecraftServer` schema by the operator; embedding it here would repeat
/// the largest schema of the chart.
fn free_form(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "object", "x-kubernetes-preserve-unknown-fields": true })
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftRestoreStatus {
    #[serde(default)]
    pub phase: JobPhase,
    /// The archive being restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A relative path with no `..`, for storage keys, sub-paths and excludes.
pub fn check_relative_path(path: &str) -> Result<(), String> {
    let ok = !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
        && path.chars().all(|c| !c.is_control());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{path:?} must be a relative path without '.', '..' or empty segments"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_lock_is_recognised() {
        let with = |v: &str| BTreeMap::from([(LOCK_ANNOTATION.to_string(), v.to_string())]);
        assert!(stop_requested(&with("stop:MinecraftRestore/a")));
        assert!(!stop_requested(&with("online:MinecraftBackup/a")));
        assert!(!stop_requested(&BTreeMap::new()));
    }

    #[test]
    fn relative_paths() {
        assert!(check_relative_path("ns/server/a.tar.gz").is_ok());
        for bad in ["", "/etc", "a/../b", "..", "a//b", "./a", "a\nb"] {
            assert!(check_relative_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn new_server_schema_is_free_form() {
        use kube::CustomResourceExt;
        let crd = serde_json::to_value(MinecraftRestore::crd()).unwrap();
        let target = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["target"];
        assert_eq!(
            target["properties"]["newServer"]["x-kubernetes-preserve-unknown-fields"],
            true
        );
    }
}
