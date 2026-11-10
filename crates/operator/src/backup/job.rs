//! Builds the Jobs and works out retention. Pure functions, so they are unit-tested.

use std::collections::BTreeMap;

use anyhow::Result;
use cubeharbor_backup::run::env;
use cubeharbor_backup::store::backend;
use cubeharbor_core::backup::{BackupStorage, JobPhase, MinecraftBackup, Retention};
use cubeharbor_core::crd::RCON_PORT;
use cubeharbor_core::{MinecraftServer, defaults, labels};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;
use serde_json::{Value, json};

use crate::config::Config;
use crate::resources::{secret_name, selector};

pub const COMPONENT: &str = "backup";
/// Backup pods carry this, not `cubeharbor.dev/server`: that label would put them behind the
/// server's Service and NetworkPolicy. The value is the server's name.
pub const BACKUP_OF_LABEL: &str = "cubeharbor.dev/backup-of";
/// On a backup Job: JSON list of the `MinecraftBackup` names to delete once it succeeds.
pub const PRUNE_ANNOTATION: &str = "cubeharbor.dev/prune-backups";
const DEFAULT_TIMEOUT_SECONDS: u32 = 6 * 60 * 60;
/// Pod retries of a `save-on` Job: about ten minutes of backoff in all.
const SAVE_ON_RETRIES: i32 = 6;

/// Did this backup Job pause saves over RCON (an online backup)? Read from the Job itself, so it
/// holds even when the operator stopped before recording the mode in the backup's status.
pub fn pauses_saves(job: &Job) -> bool {
    job.spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .is_some_and(|p| p.containers.iter().flat_map(|c| c.env.iter().flatten()).any(|e| e.name == env::RCON_ADDR))
}
/// Java version of the image the tool runs in; it doesn't use Java.
const TOOL_IMAGE_JAVA: u8 = 21;
const DATA_MOUNT: &str = defaults::DATA_DIR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Backup,
    Restore,
    SaveOn,
}

impl Action {
    fn arg(self) -> &'static str {
        match self {
            Action::Backup => "backup",
            Action::Restore => "restore",
            Action::SaveOn => "save-on",
        }
    }
}

pub struct JobInputs<'a> {
    pub name: String,
    pub action: Action,
    pub owner: OwnerReference,
    pub server: &'a MinecraftServer,
    /// Absent for [`Action::SaveOn`], which only talks to the server.
    pub storage: Option<&'a BackupStorage>,
    pub key: &'a str,
    pub exclude: &'a [String],
    /// Saves are controlled over RCON, and the pod must land on the server's node to share a
    /// ReadWriteOnce volume.
    pub online: bool,
    pub delete_keys: &'a [String],
    pub prune: &'a [String],
    pub timeout_seconds: Option<u32>,
    pub cfg: &'a Config,
}

pub fn data_pvc_name(server: &str) -> String {
    format!("data-{server}-0")
}

pub fn job_name(owner: &str, action: Action) -> String {
    format!("{owner}-{}", action.arg().replace('-', ""))
}

fn merged_tolerations(server: &MinecraftServer, cfg: &Config) -> Result<Vec<Value>> {
    let mut all = cfg.server_pod.tolerations.clone();
    for t in &server.spec.tolerations {
        let t = serde_json::to_value(t)?;
        if !all.contains(&t) {
            all.push(t);
        }
    }
    Ok(all)
}

pub fn build(inp: JobInputs<'_>) -> Result<Job> {
    let cfg = inp.cfg;
    let server = inp.server.name_any();
    let ns = inp.server.namespace().unwrap_or_default();
    let mut pod_labels = BTreeMap::from([
        (labels::COMPONENT.to_string(), COMPONENT.to_string()),
        (BACKUP_OF_LABEL.to_string(), server.clone()),
    ]);
    pod_labels.insert(labels::MANAGED_BY.into(), labels::MANAGER.into());

    let mut container_env = vec![json!({ "name": env::DATA_DIR, "value": DATA_MOUNT })];
    let mut mounts = vec![];
    let mut volumes = vec![];
    if inp.action != Action::SaveOn {
        let storage = inp
            .storage
            .ok_or_else(|| anyhow::anyhow!("a {:?} Job needs storage", inp.action))?;
        let wiring = backend(storage).map_err(anyhow::Error::msg)?.wiring();
        container_env.push(json!({ "name": env::KEY, "value": inp.key }));
        container_env
            .push(json!({ "name": env::STORAGE, "value": serde_json::to_string(storage)? }));
        container_env
            .push(json!({ "name": env::EXCLUDE, "value": serde_json::to_string(inp.exclude)? }));
        if !inp.delete_keys.is_empty() {
            container_env.push(json!({ "name": env::DELETE_KEYS, "value": serde_json::to_string(inp.delete_keys)? }));
        }
        container_env.extend(wiring.env);
        volumes.extend(wiring.volumes);
        mounts.extend(wiring.mounts);
        // Backups only read the server's data.
        mounts.push(json!({ "name": "data", "mountPath": DATA_MOUNT, "readOnly": inp.action == Action::Backup }));
        volumes.push(json!({ "name": "data", "persistentVolumeClaim": { "claimName": data_pvc_name(&server) } }));
    }
    if inp.online {
        container_env.push(
            json!({ "name": env::RCON_ADDR, "value": format!("{server}.{ns}.svc:{RCON_PORT}") }),
        );
        container_env.push(json!({ "name": env::RCON_PASSWORD, "valueFrom": { "secretKeyRef": { "name": secret_name(inp.server), "key": "RCON_PASSWORD" } } }));
    }

    let security_context = cfg.server_pod.security_context.clone().unwrap_or_else(|| {
        json!({
            "runAsUser": defaults::SERVER_UID, "runAsGroup": defaults::SERVER_UID, "runAsNonRoot": true,
            "fsGroup": defaults::SERVER_UID, "fsGroupChangePolicy": "OnRootMismatch",
            "seccompProfile": { "type": "RuntimeDefault" },
        })
    });
    let mut node_selector = cfg.server_pod.node_selector.clone();
    node_selector.extend(inp.server.spec.node_selector.clone());

    let mut pod = json!({
        "restartPolicy": "Never",
        "enableServiceLinks": false,
        // The tool needs no Kubernetes access; the operator reads its result from the pod status.
        "automountServiceAccountToken": false,
        "securityContext": security_context,
        "nodeSelector": node_selector,
        "containers": [{
            "name": "backup",
            "image": cfg.image_for(TOOL_IMAGE_JAVA),
            "imagePullPolicy": cfg.image_pull_policy,
            "command": ["cubeharbor-backup", inp.action.arg()],
            "env": container_env,
            "volumeMounts": mounts,
            "resources": serde_json::from_str::<Value>(&cfg.backup_job_resources).unwrap_or_else(|_| json!({})),
            "securityContext": {
                "allowPrivilegeEscalation": false,
                "capabilities": { "drop": ["ALL"] },
                "readOnlyRootFilesystem": true,
            },
        }],
        "volumes": volumes,
    });
    let tolerations = merged_tolerations(inp.server, cfg)?;
    if !tolerations.is_empty() {
        pod["tolerations"] = json!(tolerations);
    }
    if !cfg.server_pod.image_pull_secrets.is_empty() {
        pod["imagePullSecrets"] = json!(cfg.server_pod.image_pull_secrets);
    }
    if inp.online && inp.action == Action::Backup {
        pod["affinity"] = json!({ "podAffinity": { "requiredDuringSchedulingIgnoredDuringExecution": [{
            "labelSelector": { "matchLabels": selector(&server) },
            "topologyKey": "kubernetes.io/hostname",
        }] } });
    }

    let mut annotations = BTreeMap::new();
    if !inp.prune.is_empty() {
        annotations.insert(
            PRUNE_ANNOTATION.to_string(),
            serde_json::to_string(inp.prune)?,
        );
    }
    Ok(serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": inp.name,
            "namespace": ns,
            "labels": pod_labels,
            "annotations": annotations,
            "ownerReferences": [inp.owner],
        },
        "spec": {
            // Backups and restores are not retried blindly; `save-on` is, since autosave left off
            // silently loses progress (Kubernetes backs off 10s, 20s, 40s, ... between pods).
            "backoffLimit": if inp.action == Action::SaveOn { SAVE_ON_RETRIES } else { 0 },
            "activeDeadlineSeconds": inp.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
            "ttlSecondsAfterFinished": cfg.backup_job_ttl_seconds,
            "template": { "metadata": { "labels": pod_labels }, "spec": pod },
        },
    }))?)
}

/// `<namespace>/<server>/<backup>.tar.gz`, unique per backup.
pub fn object_key(ns: &str, server: &str, backup: &str) -> String {
    format!("{ns}/{server}/{backup}.tar.gz")
}

/// Older successful backups of a schedule that this new backup makes expendable, oldest logic
/// aside: returns those beyond `keepLast` (counting the new one) or older than `keepDays`.
/// Backups on other storage are left alone: this Job can only delete from its own.
pub fn expired<'a>(
    siblings: &'a [MinecraftBackup],
    current: &str,
    storage: &BackupStorage,
    retention: &Retention,
    now: jiff::Timestamp,
) -> Vec<&'a MinecraftBackup> {
    let mut done: Vec<&MinecraftBackup> = siblings
        .iter()
        .filter(|b| b.name_any() != current && b.spec.storage == *storage)
        .filter(|b| {
            b.status
                .as_ref()
                .is_some_and(|s| s.phase == JobPhase::Completed && s.key.is_some())
        })
        .collect();
    done.sort_by_key(|b| std::cmp::Reverse(b.metadata.creation_timestamp.clone().map(|t| t.0)));
    done.into_iter()
        .enumerate()
        .filter(|(i, b)| {
            let over_count = retention
                .keep_last
                .is_some_and(|n| i + 1 >= n.max(1) as usize);
            let too_old = retention.keep_days.is_some_and(|d| {
                let created = b
                    .metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|t| t.0.as_second())
                    .unwrap_or(i64::MAX);
                now.as_second() - created > i64::from(d) * 86_400
            });
            over_count || too_old
        })
        .map(|(_, b)| b)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use cubeharbor_core::MinecraftServerSpec;
    use cubeharbor_core::backup::{
        MinecraftBackupSpec, MinecraftBackupStatus, PvcStorage, S3CredentialsSecret, S3Storage,
    };

    fn cfg() -> Config {
        Config::parse_from(["op"])
    }

    fn server() -> MinecraftServer {
        let mut s = MinecraftServer::new("lobby", MinecraftServerSpec::default());
        s.metadata.namespace = Some("mc".into());
        s
    }

    fn pvc() -> BackupStorage {
        BackupStorage {
            pvc: Some(PvcStorage {
                claim_name: "bk".into(),
                sub_path: None,
            }),
            s3: None,
        }
    }

    fn s3() -> BackupStorage {
        let s = S3Storage {
            bucket: "b".into(),
            region: "r".into(),
            credentials_secret: S3CredentialsSecret {
                name: "creds".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        BackupStorage {
            pvc: None,
            s3: Some(s),
        }
    }

    fn job(action: Action, online: bool, storage: Option<&BackupStorage>) -> Value {
        let (srv, c) = (server(), cfg());
        let j = build(JobInputs {
            name: "x-backup".into(),
            action,
            owner: OwnerReference {
                api_version: "cubeharbor.dev/v1alpha1".into(),
                kind: "MinecraftBackup".into(),
                name: "x".into(),
                uid: "u".into(),
                ..Default::default()
            },
            server: &srv,
            storage,
            key: "mc/lobby/x.tar.gz",
            exclude: &[],
            online,
            delete_keys: &[],
            prune: &[],
            timeout_seconds: None,
            cfg: &c,
        })
        .unwrap();
        serde_json::to_value(j).unwrap()
    }

    fn env_names(v: &Value) -> Vec<String> {
        v["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn online_backup_uses_rcon_and_follows_the_server() {
        let v = job(Action::Backup, true, Some(&pvc()));
        let spec = &v["spec"]["template"]["spec"];
        assert!(env_names(&v).contains(&"RCON_ADDR".to_string()));
        let rcon = spec["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "RCON_PASSWORD")
            .unwrap();
        assert_eq!(
            rcon["valueFrom"]["secretKeyRef"]["name"],
            "lobby-cubeharbor"
        );
        assert!(spec["affinity"]["podAffinity"].is_object());
        assert_eq!(spec["automountServiceAccountToken"], false);
        assert_eq!(v["spec"]["backoffLimit"], 0);
        let data = spec["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "data")
            .unwrap();
        assert_eq!(data["readOnly"], true);
    }

    #[test]
    fn offline_and_restore_jobs() {
        let v = job(Action::Backup, false, Some(&pvc()));
        assert!(!env_names(&v).contains(&"RCON_PASSWORD".to_string()));
        assert!(v["spec"]["template"]["spec"]["affinity"].is_null());

        let v = job(Action::Restore, false, Some(&pvc()));
        let spec = &v["spec"]["template"]["spec"];
        assert_eq!(spec["containers"][0]["command"][1], "restore");
        let data = spec["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "data")
            .unwrap();
        assert_eq!(data["readOnly"], false);
    }

    #[test]
    fn backup_pods_stay_out_of_the_servers_service() {
        let v = job(Action::Backup, true, Some(&pvc()));
        let pod_labels = v["spec"]["template"]["metadata"]["labels"]
            .as_object()
            .unwrap();
        assert!(!pod_labels.contains_key(labels::SERVER));
        assert_eq!(pod_labels[BACKUP_OF_LABEL], "lobby");
    }

    #[test]
    fn s3_credentials_only_as_secret_refs() {
        let v = job(Action::Backup, false, Some(&s3()));
        let envs = v["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        for name in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ] {
            let e = envs.iter().find(|e| e["name"] == name).unwrap();
            assert_eq!(e["valueFrom"]["secretKeyRef"]["name"], "creds");
            assert!(e.get("value").is_none());
        }
    }

    #[test]
    fn only_save_on_is_retried_and_online_backups_are_recognised() {
        let s = pvc();
        let online = job(Action::Backup, true, Some(&s));
        assert_eq!(online["spec"]["backoffLimit"], 0);
        assert_eq!(job(Action::SaveOn, true, None)["spec"]["backoffLimit"], SAVE_ON_RETRIES);
        let typed = |v: Value| serde_json::from_value::<Job>(v).unwrap();
        assert!(pauses_saves(&typed(online)));
        assert!(!pauses_saves(&typed(job(Action::Backup, false, Some(&s)))));
    }

    #[test]
    fn save_on_job_has_no_volumes() {
        let v = job(Action::SaveOn, true, None);
        assert!(
            v["spec"]["template"]["spec"]["volumes"]
                .as_array()
                .is_none_or(|a| a.is_empty())
        );
        assert_eq!(
            v["spec"]["template"]["spec"]["containers"][0]["command"][1],
            "save-on"
        );
    }

    fn sibling(
        name: &str,
        minutes_ago: i64,
        phase: JobPhase,
        storage: BackupStorage,
    ) -> MinecraftBackup {
        let mut b = MinecraftBackup::new(
            name,
            MinecraftBackupSpec {
                server: "lobby".into(),
                storage,
                ..Default::default()
            },
        );
        let at = jiff::Timestamp::from_second(1_000_000 - minutes_ago * 60).unwrap();
        b.metadata.creation_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(at));
        b.status = Some(MinecraftBackupStatus {
            phase,
            key: Some(format!("k/{name}")),
            ..Default::default()
        });
        b
    }

    fn names(v: Vec<&MinecraftBackup>) -> Vec<String> {
        v.into_iter().map(|b| b.name_any()).collect()
    }

    #[test]
    fn keep_last_counts_the_new_backup() {
        let all = vec![
            sibling("a", 30, JobPhase::Completed, pvc()),
            sibling("b", 20, JobPhase::Completed, pvc()),
            sibling("c", 10, JobPhase::Completed, pvc()),
            sibling("new", 0, JobPhase::Running, pvc()),
        ];
        let now = jiff::Timestamp::from_second(1_000_000).unwrap();
        let r = Retention {
            keep_last: Some(2),
            keep_days: None,
        };
        assert_eq!(names(expired(&all, "new", &pvc(), &r, now)), ["b", "a"]);
        assert!(expired(&all, "new", &pvc(), &Retention::default(), now).is_empty());
    }

    #[test]
    fn keep_days_and_unrelated_backups() {
        let week = 7 * 24 * 60;
        let all = vec![
            sibling("old", week + 1, JobPhase::Completed, pvc()),
            sibling("recent", 60, JobPhase::Completed, pvc()),
            sibling("failed", week + 1, JobPhase::Failed, pvc()),
            sibling("elsewhere", week + 1, JobPhase::Completed, s3()),
        ];
        let now = jiff::Timestamp::from_second(1_000_000).unwrap();
        let r = Retention {
            keep_last: None,
            keep_days: Some(7),
        };
        assert_eq!(names(expired(&all, "new", &pvc(), &r, now)), ["old"]);
    }
}
