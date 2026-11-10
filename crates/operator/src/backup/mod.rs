//! Backup, schedule and restore controllers. Only started with `BACKUPS_ENABLED`.
//!
//! Each backup or restore runs as a Job using the `cubeharbor-backup` tool, because only a pod
//! can mount the server's volume. A per-server lock annotation stops two of them overlapping and,
//! when the server must be down, tells the server reconciler to keep it at zero replicas.

mod job;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cubeharbor_backup::cron::Cron;
use cubeharbor_backup::run::Report;
use cubeharbor_backup::store::backend;
use cubeharbor_core::backup::{
    BackupStorage, JobPhase, LOCK_ANNOTATION, LOCK_ONLINE, LOCK_STOP, MinecraftBackup,
    MinecraftBackupSchedule, MinecraftBackupScheduleStatus, MinecraftBackupSpec,
    MinecraftBackupStatus, MinecraftRestore, MinecraftRestoreStatus, SCHEDULE_LABEL,
    check_relative_path,
};
use cubeharbor_core::crd::check_server_name;
use cubeharbor_core::{MinecraftServer, MinecraftServerSpec, labels};
use futures::StreamExt;
use jiff::Timestamp;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{ListParams, Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::{Controller, watcher};
use kube::{Api, Client, Resource, ResourceExt};
use serde_json::json;

use self::job::{
    Action as JobAction, BACKUP_OF_LABEL, JobInputs, PRUNE_ANNOTATION, data_pvc_name, job_name,
    object_key,
};
use crate::config::Config;
use crate::resources::selector;

/// A backup or restore waiting this long for its server gives up.
const WAIT_LIMIT_SECONDS: i64 = 30 * 60;
/// A schedule missed by more than this (operator down) is skipped until the next fire time.
const MISSED_WINDOW_SECONDS: i64 = 60 * 60;
const KEEP_FAILED: usize = 3;
const POLL: Duration = Duration::from_secs(15);
const LOCK_SWEEP: Duration = Duration::from_secs(60);

pub struct BackupCtx {
    pub client: Client,
    pub cfg: Config,
}

#[derive(Debug, thiserror::Error)]
#[error("{0:#}")]
pub struct Error(#[from] anyhow::Error);

impl From<kube::Error> for Error {
    fn from(e: kube::Error) -> Self {
        Error(e.into())
    }
}

fn error_policy<K>(_: Arc<K>, err: &Error, _: Arc<BackupCtx>) -> Action {
    tracing::warn!("backup reconcile: {err}");
    Action::requeue(Duration::from_secs(30))
}

fn now_string() -> String {
    Timestamp::now().to_string()
}

fn scoped<K>(client: &Client, watch_ns: &str) -> Api<K>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>
        + serde::de::DeserializeOwned
        + Clone
        + std::fmt::Debug,
{
    if watch_ns.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), watch_ns)
    }
}

async fn log_result<T, E: std::fmt::Display>(res: Result<T, E>) {
    if let Err(e) = res {
        tracing::debug!("backup controller: {e}");
    }
}

/// Run the controllers until the process exits.
pub async fn run(client: Client, cfg: Config) {
    let ctx = Arc::new(BackupCtx {
        client: client.clone(),
        cfg: cfg.clone(),
    });
    let ns = cfg.watch_namespace.as_str();
    let owned =
        watcher::Config::default().labels(&format!("{}={}", labels::COMPONENT, job::COMPONENT));

    let backups = Controller::new(
        scoped::<MinecraftBackup>(&client, ns),
        watcher::Config::default(),
    )
    .owns(scoped::<Job>(&client, ns), owned.clone())
    .run(reconcile_backup, error_policy, ctx.clone())
    .for_each(log_result);
    let restores = Controller::new(
        scoped::<MinecraftRestore>(&client, ns),
        watcher::Config::default(),
    )
    .owns(scoped::<Job>(&client, ns), owned)
    .run(reconcile_restore, error_policy, ctx.clone())
    .for_each(log_result);
    let schedules = Controller::new(
        scoped::<MinecraftBackupSchedule>(&client, ns),
        watcher::Config::default(),
    )
    .run(reconcile_schedule, error_policy, ctx.clone())
    .for_each(log_result);
    let sweep = sweep_locks(ctx);
    tokio::join!(backups, restores, schedules, sweep);
}

fn owner_of<K: Resource<DynamicType = ()>>(obj: &K) -> Result<OwnerReference> {
    obj.controller_owner_ref(&())
        .ok_or_else(|| anyhow!("object has no uid yet"))
}

fn lock_value(mode: &str, kind: &str, name: &str) -> String {
    format!("{mode}:{kind}/{name}")
}

/// Take the lock on a server. Returns false when something else holds it or won the race.
async fn acquire_lock(
    api: &Api<MinecraftServer>,
    server: &MinecraftServer,
    value: &str,
) -> Result<bool> {
    match server.annotations().get(LOCK_ANNOTATION) {
        Some(v) if v == value => return Ok(true),
        Some(_) => return Ok(false),
        None => {}
    }
    // Merge patch with the resourceVersion: a conflicting writer makes this fail instead of overwrite.
    let patch = json!({ "metadata": { "resourceVersion": server.resource_version(), "annotations": { LOCK_ANNOTATION: value } } });
    match api
        .patch(
            &server.name_any(),
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Release the lock if `owner` (the part after the mode) still holds it.
async fn release_lock(api: &Api<MinecraftServer>, server_name: &str, owner: &str) -> Result<()> {
    let Some(server) = api.get_opt(server_name).await? else {
        return Ok(());
    };
    let held = server
        .annotations()
        .get(LOCK_ANNOTATION)
        .and_then(|v| v.split_once(':'))
        .is_some_and(|(_, o)| o == owner);
    if !held {
        return Ok(());
    }
    let patch = json!({ "metadata": { "annotations": { LOCK_ANNOTATION: null } } });
    api.patch(server_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

fn job_finished(job: &Job) -> Option<bool> {
    let status = job.status.as_ref()?;
    if status.succeeded.unwrap_or(0) > 0 {
        return Some(true);
    }
    let failed_condition = status
        .conditions
        .iter()
        .flatten()
        .any(|c| c.type_ == "Failed" && c.status == "True");
    (failed_condition || status.failed.unwrap_or(0) > 0).then_some(false)
}

/// The tool's report, from the termination message of the Job's pod.
async fn job_report(pods: &Api<Pod>, job_name: &str) -> Option<Report> {
    let list = pods
        .list(&ListParams::default().labels(&format!("batch.kubernetes.io/job-name={job_name}")))
        .await
        .ok()?;
    list.items
        .iter()
        .flat_map(|p| {
            p.status
                .iter()
                .flat_map(|s| s.container_statuses.iter().flatten())
        })
        .find_map(|c| {
            c.state
                .as_ref()?
                .terminated
                .as_ref()?
                .message
                .as_deref()
                .and_then(|m| serde_json::from_str(m).ok())
        })
}

fn seconds_since_created<K: Resource>(obj: &K) -> i64 {
    obj.meta()
        .creation_timestamp
        .as_ref()
        .map_or(0, |t| Timestamp::now().as_second() - t.0.as_second())
}

/// A pod that isn't there, is ready, or is neither.
enum ServerState {
    Stopped,
    Ready,
    Busy,
}

fn pod_state(pod: Option<&Pod>) -> ServerState {
    let Some(pod) = pod else {
        return ServerState::Stopped;
    };
    let ready = pod.metadata.deletion_timestamp.is_none()
        && pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
    if ready {
        ServerState::Ready
    } else {
        ServerState::Busy
    }
}

/// True once no replica of the server can come back while we work on its volume.
async fn scaled_down(ctx: &BackupCtx, ns: &str, server: &str) -> Result<bool> {
    let sts = Api::<StatefulSet>::namespaced(ctx.client.clone(), ns)
        .get_opt(server)
        .await?;
    let zero = sts.is_none_or(|s| s.spec.replicas.unwrap_or(0) == 0);
    let pod = Api::<Pod>::namespaced(ctx.client.clone(), ns)
        .get_opt(&format!("{server}-0"))
        .await?;
    Ok(zero && pod.is_none())
}

/// The server's data volume, created up front for a server that has never run. Same name and
/// shape as the StatefulSet's claim template, so the StatefulSet adopts it.
async fn ensure_data_pvc(ctx: &BackupCtx, ns: &str, server: &MinecraftServer) -> Result<()> {
    let api = Api::<PersistentVolumeClaim>::namespaced(ctx.client.clone(), ns);
    let name = data_pvc_name(&server.name_any());
    if api.get_opt(&name).await?.is_some() {
        return Ok(());
    }
    let mut spec = json!({
        "accessModes": ["ReadWriteOnce"],
        "resources": { "requests": { "storage": server.spec.resources.storage } },
    });
    if let Some(sc) = server
        .spec
        .resources
        .storage_class
        .clone()
        .or(ctx.cfg.default_storage_class.clone())
    {
        spec["storageClassName"] = json!(sc);
    }
    let pvc: PersistentVolumeClaim = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": { "name": name, "namespace": ns, "labels": selector(&server.name_any()) },
        "spec": spec,
    }))?;
    match api.create(&PostParams::default(), &pvc).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn create_job(api: &Api<Job>, job: &Job) -> Result<()> {
    match api.create(&PostParams::default(), job).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `status`, with `message` always present: a merge patch only clears a field sent as null.
fn status_patch(
    status: &impl serde::Serialize,
    message: Option<&str>,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(status)?;
    value["message"] = json!(message);
    Ok(json!({ "status": value }))
}

async fn patch_backup_status(
    api: &Api<MinecraftBackup>,
    name: &str,
    status: &MinecraftBackupStatus,
) -> Result<()> {
    api.patch_status(
        name,
        &PatchParams::default(),
        &Patch::Merge(status_patch(status, status.message.as_deref())?),
    )
    .await?;
    Ok(())
}

async fn reconcile_backup(
    backup: Arc<MinecraftBackup>,
    ctx: Arc<BackupCtx>,
) -> Result<Action, Error> {
    let ns = backup.namespace().unwrap_or_default();
    let name = backup.name_any();
    let api = Api::<MinecraftBackup>::namespaced(ctx.client.clone(), &ns);
    let servers = Api::<MinecraftServer>::namespaced(ctx.client.clone(), &ns);
    let owner = format!("MinecraftBackup/{name}");
    let status = backup.status.clone().unwrap_or_default();

    if status.phase.finished() {
        release_lock(&servers, &backup.spec.server, &owner).await?;
        return Ok(Action::await_change());
    }
    let fail = |message: String| MinecraftBackupStatus {
        phase: JobPhase::Failed,
        completed_at: Some(now_string()),
        message: Some(message),
        ..status.clone()
    };

    let jobs = Api::<Job>::namespaced(ctx.client.clone(), &ns);
    let jname = job_name(&name, JobAction::Backup);
    if let Some(job) = jobs.get_opt(&jname).await? {
        return observe_backup_job(&ctx, &backup, &job, &status).await;
    }
    if status.phase == JobPhase::Running {
        // Deleted, or cleaned up by its TTL while the operator was down: nothing says whether it
        // turned saves back on, so make sure.
        if status.mode.as_deref() == Some("online")
            && let Some(server) = servers.get_opt(&backup.spec.server).await?
        {
            recover_saves(&ctx, &backup, &server).await.context("creating the save-on Job")?;
        }
        release_lock(&servers, &backup.spec.server, &owner).await?;
        patch_backup_status(&api, &name, &fail("the backup Job disappeared".into())).await?;
        return Ok(Action::await_change());
    }

    if let Err(e) = validate_backup(&backup.spec) {
        patch_backup_status(&api, &name, &fail(e)).await?;
        return Ok(Action::await_change());
    }
    let Some(server) = servers.get_opt(&backup.spec.server).await? else {
        patch_backup_status(
            &api,
            &name,
            &fail(format!("server {} not found", backup.spec.server)),
        )
        .await?;
        return Ok(Action::await_change());
    };
    let give_up = seconds_since_created(backup.as_ref()) > WAIT_LIMIT_SECONDS;
    let pod = Api::<Pod>::namespaced(ctx.client.clone(), &ns)
        .get_opt(&format!("{}-0", backup.spec.server))
        .await?;
    let stop_held = server
        .annotations()
        .get(LOCK_ANNOTATION)
        .is_some_and(|v| *v == lock_value(LOCK_STOP, "MinecraftBackup", &name));

    let online = match (pod_state(pod.as_ref()), stop_held) {
        (ServerState::Ready, false) => {
            if !acquire_lock(
                &servers,
                &server,
                &lock_value(LOCK_ONLINE, "MinecraftBackup", &name),
            )
            .await?
            {
                return wait_backup(
                    &api,
                    &backup,
                    &status,
                    give_up,
                    "waiting for another backup or restore of this server",
                )
                .await;
            }
            true
        }
        (ServerState::Stopped, _) => {
            if !acquire_lock(
                &servers,
                &server,
                &lock_value(LOCK_STOP, "MinecraftBackup", &name),
            )
            .await?
            {
                return wait_backup(
                    &api,
                    &backup,
                    &status,
                    give_up,
                    "waiting for another backup or restore of this server",
                )
                .await;
            }
            // The lock has to reach the StatefulSet before the volume is safe to mount.
            if !scaled_down(&ctx, &ns, &backup.spec.server).await? {
                return wait_backup(
                    &api,
                    &backup,
                    &status,
                    give_up,
                    "waiting for the server to stop",
                )
                .await;
            }
            false
        }
        _ => {
            return wait_backup(
                &api,
                &backup,
                &status,
                give_up,
                "waiting for the server to be ready or stopped",
            )
            .await;
        }
    };

    let key = object_key(&ns, &backup.spec.server, &name);
    let (delete_keys, prune) = retention_for(&ctx, &backup, &ns).await?;
    let job = job::build(JobInputs {
        name: jname,
        action: JobAction::Backup,
        owner: owner_of(backup.as_ref())?,
        server: &server,
        storage: Some(&backup.spec.storage),
        key: &key,
        exclude: &backup.spec.exclude,
        online,
        delete_keys: &delete_keys,
        prune: &prune,
        timeout_seconds: backup.spec.timeout_seconds,
        cfg: &ctx.cfg,
    })?;
    create_job(&jobs, &job).await?;
    let running = MinecraftBackupStatus {
        phase: JobPhase::Running,
        mode: Some(if online { "online" } else { "offline" }.into()),
        key: Some(key),
        started_at: Some(now_string()),
        message: None,
        ..status
    };
    patch_backup_status(&api, &name, &running).await?;
    Ok(Action::requeue(POLL))
}

fn validate_backup(spec: &MinecraftBackupSpec) -> Result<(), String> {
    backend(&spec.storage)?;
    spec.exclude.iter().try_for_each(|e| check_relative_path(e))
}

async fn wait_backup(
    api: &Api<MinecraftBackup>,
    backup: &MinecraftBackup,
    status: &MinecraftBackupStatus,
    give_up: bool,
    message: &str,
) -> Result<Action, Error> {
    if give_up {
        let failed = MinecraftBackupStatus {
            phase: JobPhase::Failed,
            completed_at: Some(now_string()),
            message: Some(format!("gave up: {message}")),
            ..status.clone()
        };
        patch_backup_status(api, &backup.name_any(), &failed).await?;
        return Ok(Action::await_change());
    }
    let pending = MinecraftBackupStatus {
        phase: JobPhase::Pending,
        message: Some(message.into()),
        ..status.clone()
    };
    if status.message.as_deref() != Some(message) {
        patch_backup_status(api, &backup.name_any(), &pending).await?;
    }
    Ok(Action::requeue(POLL))
}

/// What this backup's schedule says can go once it succeeds: storage keys and object names.
async fn retention_for(
    ctx: &BackupCtx,
    backup: &MinecraftBackup,
    ns: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let Some(schedule_name) = backup.labels().get(SCHEDULE_LABEL) else {
        return Ok((vec![], vec![]));
    };
    let Some(schedule) = Api::<MinecraftBackupSchedule>::namespaced(ctx.client.clone(), ns)
        .get_opt(schedule_name)
        .await?
    else {
        return Ok((vec![], vec![]));
    };
    let api = Api::<MinecraftBackup>::namespaced(ctx.client.clone(), ns);
    let siblings = api
        .list(&ListParams::default().labels(&format!("{SCHEDULE_LABEL}={schedule_name}")))
        .await?
        .items;
    let gone = job::expired(
        &siblings,
        &backup.name_any(),
        &backup.spec.storage,
        &schedule.spec.retention,
        Timestamp::now(),
    );
    let keys = gone
        .iter()
        .filter_map(|b| b.status.as_ref()?.key.clone())
        .collect();
    let names = gone.iter().map(|b| b.name_any()).collect();
    Ok((keys, names))
}

async fn observe_backup_job(
    ctx: &BackupCtx,
    backup: &MinecraftBackup,
    job: &Job,
    status: &MinecraftBackupStatus,
) -> Result<Action, Error> {
    let ns = backup.namespace().unwrap_or_default();
    let name = backup.name_any();
    let api = Api::<MinecraftBackup>::namespaced(ctx.client.clone(), &ns);
    let Some(succeeded) = job_finished(job) else {
        return Ok(Action::requeue(POLL));
    };
    let report = job_report(&Api::namespaced(ctx.client.clone(), &ns), &job.name_any()).await;
    let servers = Api::<MinecraftServer>::namespaced(ctx.client.clone(), &ns);

    let mut next = MinecraftBackupStatus {
        completed_at: Some(now_string()),
        ..status.clone()
    };
    if succeeded {
        next.phase = JobPhase::Completed;
        next.size_bytes = report.as_ref().and_then(|r| r.size_bytes);
        next.sha256 = report.as_ref().and_then(|r| r.sha256.clone());
        next.message = None;
        let pruned: Vec<String> = job
            .annotations()
            .get(PRUNE_ANNOTATION)
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default();
        for old in pruned {
            match api.delete(&old, &Default::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => return Err(Error::from(e)),
            }
        }
    } else {
        next.phase = JobPhase::Failed;
        next.message = Some(
            report
                .as_ref()
                .and_then(|r| r.error.clone())
                .unwrap_or_else(|| "the backup Job failed; see its logs".into()),
        );
        // An online backup that died uncleanly may have left autosave off, which loses progress
        // silently, so turn it back on from a fresh pod.
        if (status.mode.as_deref() == Some("online") || job::pauses_saves(job))
            && !report.as_ref().is_some_and(|r| r.saves_restored)
            && let Some(server) = servers.get_opt(&backup.spec.server).await?
        {
            recover_saves(ctx, backup, &server)
                .await
                .context("creating the save-on Job")?;
        }
    }
    release_lock(
        &servers,
        &backup.spec.server,
        &format!("MinecraftBackup/{name}"),
    )
    .await?;
    patch_backup_status(&api, &name, &next).await?;
    Ok(Action::await_change())
}

async fn recover_saves(
    ctx: &BackupCtx,
    backup: &MinecraftBackup,
    server: &MinecraftServer,
) -> Result<()> {
    let job = job::build(JobInputs {
        name: job_name(&backup.name_any(), JobAction::SaveOn),
        action: JobAction::SaveOn,
        owner: owner_of(backup)?,
        server,
        storage: None,
        key: "",
        exclude: &[],
        online: true,
        delete_keys: &[],
        prune: &[],
        timeout_seconds: Some(600),
        cfg: &ctx.cfg,
    })?;
    create_job(
        &Api::namespaced(ctx.client.clone(), &backup.namespace().unwrap_or_default()),
        &job,
    )
    .await
}

async fn patch_restore_status(
    api: &Api<MinecraftRestore>,
    name: &str,
    status: &MinecraftRestoreStatus,
) -> Result<()> {
    api.patch_status(
        name,
        &PatchParams::default(),
        &Patch::Merge(status_patch(status, status.message.as_deref())?),
    )
    .await?;
    Ok(())
}

/// Where a restore reads from: the backup it names, or an explicit storage and key.
async fn restore_source(
    restore: &MinecraftRestore,
    backups: &Api<MinecraftBackup>,
) -> Result<Result<(BackupStorage, String), String>> {
    let source = &restore.spec.source;
    match (&source.backup, &source.storage, &source.key) {
        (Some(name), None, None) => {
            let Some(backup) = backups.get_opt(name).await? else {
                return Ok(Err(format!("backup {name} not found")));
            };
            let status = backup.status.unwrap_or_default();
            match (status.phase, status.key) {
                (JobPhase::Completed, Some(key)) => Ok(Ok((backup.spec.storage, key))),
                _ => Ok(Err(format!("backup {name} has not completed"))),
            }
        }
        (None, Some(storage), Some(key)) => Ok(check_relative_path(key)
            .and_then(|()| backend(storage).map(|_| ()))
            .map(|()| (storage.clone(), key.clone()))),
        _ => Ok(Err(
            "source must set either backup, or both storage and key".into(),
        )),
    }
}

async fn reconcile_restore(
    restore: Arc<MinecraftRestore>,
    ctx: Arc<BackupCtx>,
) -> Result<Action, Error> {
    let ns = restore.namespace().unwrap_or_default();
    let name = restore.name_any();
    let target = restore.spec.target.server.clone();
    let api = Api::<MinecraftRestore>::namespaced(ctx.client.clone(), &ns);
    let servers = Api::<MinecraftServer>::namespaced(ctx.client.clone(), &ns);
    let owner = format!("MinecraftRestore/{name}");
    let status = restore.status.clone().unwrap_or_default();

    if status.phase.finished() {
        release_lock(&servers, &target, &owner).await?;
        return Ok(Action::await_change());
    }
    let failed = |message: String| MinecraftRestoreStatus {
        phase: JobPhase::Failed,
        completed_at: Some(now_string()),
        message: Some(message),
        ..status.clone()
    };

    let jobs = Api::<Job>::namespaced(ctx.client.clone(), &ns);
    let jname = job_name(&name, JobAction::Restore);
    if let Some(job) = jobs.get_opt(&jname).await? {
        let Some(succeeded) = job_finished(&job) else {
            return Ok(Action::requeue(POLL));
        };
        let mut next = MinecraftRestoreStatus {
            completed_at: Some(now_string()),
            ..status.clone()
        };
        if succeeded {
            next.phase = JobPhase::Completed;
            next.message = None;
        } else {
            let report = job_report(&Api::namespaced(ctx.client.clone(), &ns), &jname).await;
            next.phase = JobPhase::Failed;
            next.message = Some(
                report
                    .and_then(|r| r.error)
                    .unwrap_or_else(|| "the restore Job failed; see its logs".into()),
            );
        }
        release_lock(&servers, &target, &owner).await?;
        patch_restore_status(&api, &name, &next).await?;
        return Ok(Action::await_change());
    }
    if status.phase == JobPhase::Running {
        patch_restore_status(&api, &name, &failed("the restore Job disappeared".into())).await?;
        return Ok(Action::await_change());
    }

    let backups = Api::<MinecraftBackup>::namespaced(ctx.client.clone(), &ns);
    let (storage, key) = match restore_source(&restore, &backups).await? {
        Ok(v) => v,
        Err(e) => {
            patch_restore_status(&api, &name, &failed(e)).await?;
            return Ok(Action::await_change());
        }
    };
    let lock = lock_value(LOCK_STOP, "MinecraftRestore", &name);
    let wait = |message: &str| MinecraftRestoreStatus {
        phase: JobPhase::Pending,
        key: Some(key.clone()),
        message: Some(message.into()),
        ..status.clone()
    };
    let give_up = seconds_since_created(restore.as_ref()) > WAIT_LIMIT_SECONDS;

    let server = match servers.get_opt(&target).await? {
        Some(s) => {
            if !restore.spec.target.overwrite_existing
                && s.annotations().get(LOCK_ANNOTATION) != Some(&lock)
            {
                patch_restore_status(&api, &name, &failed(format!("server {target} already exists; set target.overwriteExisting to replace its data"))).await?;
                return Ok(Action::await_change());
            }
            s
        }
        None => match new_server(&restore, &target, &ns, &lock) {
            Ok(s) => {
                match servers.create(&PostParams::default(), &s).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(e)) if e.code == 409 => {}
                    Err(e) => return Err(Error::from(e)),
                }
                return Ok(Action::requeue(Duration::from_secs(3)));
            }
            Err(e) => {
                patch_restore_status(&api, &name, &failed(e)).await?;
                return Ok(Action::await_change());
            }
        },
    };

    let message = if !acquire_lock(&servers, &server, &lock).await? {
        Some("waiting for another backup or restore of this server")
    } else if !scaled_down(&ctx, &ns, &target).await? {
        Some("waiting for the server to stop")
    } else {
        None
    };
    if let Some(message) = message {
        if give_up {
            release_lock(&servers, &target, &owner).await?;
            patch_restore_status(&api, &name, &failed(format!("gave up: {message}"))).await?;
            return Ok(Action::await_change());
        }
        if status.message.as_deref() != Some(message) {
            patch_restore_status(&api, &name, &wait(message)).await?;
        }
        return Ok(Action::requeue(POLL));
    }

    ensure_data_pvc(&ctx, &ns, &server).await?;
    let job = job::build(JobInputs {
        name: jname,
        action: JobAction::Restore,
        owner: owner_of(restore.as_ref())?,
        server: &server,
        storage: Some(&storage),
        key: &key,
        exclude: &[],
        online: false,
        delete_keys: &[],
        prune: &[],
        timeout_seconds: restore.spec.timeout_seconds,
        cfg: &ctx.cfg,
    })?;
    create_job(&jobs, &job).await?;
    let running = MinecraftRestoreStatus {
        phase: JobPhase::Running,
        key: Some(key.clone()),
        started_at: Some(now_string()),
        message: None,
        ..status.clone()
    };
    patch_restore_status(&api, &name, &running).await?;
    Ok(Action::requeue(POLL))
}

/// The `MinecraftServer` a restore creates, already locked so it never starts on empty data.
fn new_server(
    restore: &MinecraftRestore,
    name: &str,
    ns: &str,
    lock: &str,
) -> Result<MinecraftServer, String> {
    let raw = restore
        .spec
        .target
        .new_server
        .clone()
        .ok_or_else(|| format!("server {name} not found; set target.newServer to create it"))?;
    check_server_name(name)?;
    let spec: MinecraftServerSpec =
        serde_json::from_value(raw).map_err(|e| format!("target.newServer: {e}"))?;
    spec.check_ports()?;
    let mut server = MinecraftServer::new(name, spec);
    server.metadata.namespace = Some(ns.to_string());
    server
        .annotations_mut()
        .insert(LOCK_ANNOTATION.to_string(), lock.to_string());
    Ok(server)
}

fn scheduled_name(schedule: &str, at: Timestamp) -> String {
    format!("{schedule}-{}", at.as_second())
}

/// Should a backup be created for the fire time `latest`?
fn is_due(latest: Timestamp, baseline: Timestamp, now: Timestamp) -> bool {
    latest > baseline && now.as_second() - latest.as_second() <= MISSED_WINDOW_SECONDS
}

async fn reconcile_schedule(
    schedule: Arc<MinecraftBackupSchedule>,
    ctx: Arc<BackupCtx>,
) -> Result<Action, Error> {
    let ns = schedule.namespace().unwrap_or_default();
    let name = schedule.name_any();
    let api = Api::<MinecraftBackupSchedule>::namespaced(ctx.client.clone(), &ns);
    let backups = Api::<MinecraftBackup>::namespaced(ctx.client.clone(), &ns);
    let previous = schedule.status.clone().unwrap_or_default();
    let mut status = previous.clone();
    status.message = None;

    let cron = match Cron::parse(&schedule.spec.schedule) {
        Ok(c) => c,
        Err(e) => {
            status.message = Some(format!("invalid schedule: {e}"));
            status.next_schedule_time = None;
            return finish_schedule(&api, &name, &previous, status, Action::await_change()).await;
        }
    };
    if let Err(e) = backend(&schedule.spec.storage) {
        status.message = Some(e);
        status.next_schedule_time = None;
        return finish_schedule(&api, &name, &previous, status, Action::await_change()).await;
    }

    let children = backups
        .list(&ListParams::default().labels(&format!("{SCHEDULE_LABEL}={name}")))
        .await?
        .items;
    let phase = |b: &MinecraftBackup| b.status.as_ref().map_or(JobPhase::Pending, |s| s.phase);
    let active = children
        .iter()
        .find(|b| !phase(b).finished())
        .map(|b| b.name_any());
    let created = |b: &MinecraftBackup| b.metadata.creation_timestamp.as_ref().map(|t| t.0);
    status.last_successful_backup = children
        .iter()
        .filter(|b| phase(b) == JobPhase::Completed)
        .max_by_key(|b| created(b))
        .map(|b| b.name_any())
        .or(status.last_successful_backup);
    status.active_backup = active.clone();

    // Failed backups have no archive; keep a few for their messages.
    let mut failed: Vec<&MinecraftBackup> = children
        .iter()
        .filter(|b| phase(b) == JobPhase::Failed)
        .collect();
    failed.sort_by_key(|b| std::cmp::Reverse(created(b)));
    for old in failed.iter().skip(KEEP_FAILED) {
        match backups.delete(&old.name_any(), &Default::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => return Err(Error::from(e)),
        }
    }

    let now = Timestamp::now();
    status.next_schedule_time = if schedule.spec.suspend {
        None
    } else {
        cron.next_after(now).map(|t| t.to_string())
    };
    if schedule.spec.suspend {
        status.message = Some("suspended".into());
        return finish_schedule(&api, &name, &previous, status, Action::await_change()).await;
    }

    let baseline = previous
        .last_schedule_time
        .as_deref()
        .and_then(|t| t.parse().ok())
        .or(schedule.metadata.creation_timestamp.as_ref().map(|t| t.0))
        .unwrap_or(now);
    if let Some(latest) = cron.latest_at(now).filter(|t| is_due(*t, baseline, now)) {
        status.last_schedule_time = Some(latest.to_string());
        if let Some(a) = &active {
            status.message = Some(format!("skipped {latest}: {a} is still running"));
        } else {
            let child_name = scheduled_name(&name, latest);
            let mut child = MinecraftBackup::new(
                &child_name,
                MinecraftBackupSpec {
                    server: schedule.spec.server.clone(),
                    storage: schedule.spec.storage.clone(),
                    exclude: schedule.spec.exclude.clone(),
                    timeout_seconds: None,
                },
            );
            child.metadata.namespace = Some(ns.clone());
            child
                .labels_mut()
                .insert(SCHEDULE_LABEL.to_string(), name.clone());
            child
                .owner_references_mut()
                .push(owner_of(schedule.as_ref())?);
            match backups.create(&PostParams::default(), &child).await {
                Ok(_) => status.active_backup = Some(child_name),
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                Err(e) => return Err(Error::from(e)),
            }
        }
    }

    let until_next = cron.next_after(now).map_or(60, |t| {
        (t.as_second() - now.as_second()).clamp(1, 60) as u64
    });
    finish_schedule(
        &api,
        &name,
        &previous,
        status,
        Action::requeue(Duration::from_secs(until_next)),
    )
    .await
}

async fn finish_schedule(
    api: &Api<MinecraftBackupSchedule>,
    name: &str,
    previous: &MinecraftBackupScheduleStatus,
    status: MinecraftBackupScheduleStatus,
    action: Action,
) -> Result<Action, Error> {
    if *previous != status {
        // Every field, so a merge patch also clears the ones that became empty.
        let patch = json!({ "status": {
            "lastScheduleTime": status.last_schedule_time,
            "nextScheduleTime": status.next_schedule_time,
            "lastSuccessfulBackup": status.last_successful_backup,
            "activeBackup": status.active_backup,
            "message": status.message,
        } });
        api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(Error::from)?;
    }
    Ok(action)
}

/// Is the lock's owner gone or finished? (`None` when the value can't be parsed.)
async fn lock_is_stale(client: &Client, ns: &str, lock: &str) -> Result<bool> {
    let Some((kind, name)) = lock.split_once(':').and_then(|(_, o)| o.split_once('/')) else {
        return Ok(true);
    };
    let jobs = Api::<Job>::namespaced(client.clone(), ns);
    let finished = match kind {
        "MinecraftBackup" => Api::<MinecraftBackup>::namespaced(client.clone(), ns)
            .get_opt(name)
            .await?
            .map(|o| o.status.is_some_and(|s| s.phase.finished())),
        "MinecraftRestore" => Api::<MinecraftRestore>::namespaced(client.clone(), ns)
            .get_opt(name)
            .await?
            .map(|o| o.status.is_some_and(|s| s.phase.finished())),
        _ => return Ok(true),
    };
    match finished {
        Some(done) => Ok(done),
        // The owner was deleted: wait until its Job is gone too.
        None => {
            let job = if kind == "MinecraftBackup" {
                JobAction::Backup
            } else {
                JobAction::Restore
            };
            Ok(jobs.get_opt(&job_name(name, job)).await?.is_none())
        }
    }
}

/// Clears locks left by a backup or restore that was deleted while it ran, which would otherwise
/// keep its server stopped for good.
async fn sweep_locks(ctx: Arc<BackupCtx>) {
    let api = scoped::<MinecraftServer>(&ctx.client, &ctx.cfg.watch_namespace);
    loop {
        tokio::time::sleep(LOCK_SWEEP).await;
        let servers = match api.list(&ListParams::default()).await {
            Ok(l) => l.items,
            Err(e) => {
                tracing::warn!("sweeping backup locks: {e}");
                continue;
            }
        };
        for server in servers {
            let Some(lock) = server.annotations().get(LOCK_ANNOTATION).cloned() else {
                continue;
            };
            let ns = server.namespace().unwrap_or_default();
            let name = server.name_any();
            match lock_is_stale(&ctx.client, &ns, &lock).await {
                Ok(true) => {
                    tracing::info!("{ns}/{name}: clearing stale backup lock {lock}");
                    let patch = json!({ "metadata": { "annotations": { LOCK_ANNOTATION: null } } });
                    if let Err(e) = Api::<MinecraftServer>::namespaced(ctx.client.clone(), &ns)
                        .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await
                    {
                        tracing::warn!("{ns}/{name}: clearing lock: {e}");
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("{ns}/{name}: checking lock: {e:#}"),
            }
        }
    }
}

/// Label on pods allowed to reach a server's RCON port.
pub fn rcon_client_labels(server: &str) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        (labels::COMPONENT.to_string(), job::COMPONENT.to_string()),
        (BACKUP_OF_LABEL.to_string(), server.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubeharbor_core::backup::{MinecraftRestoreSpec, RestoreTarget};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn due_only_once_and_only_recently() {
        let fire = ts("2026-01-01T04:00:00Z");
        let shortly = ts("2026-01-01T04:00:30Z");
        assert!(is_due(fire, ts("2026-01-01T03:00:00Z"), shortly));
        assert!(!is_due(fire, fire, shortly), "already done");
        assert!(
            !is_due(fire, ts("2026-01-01T03:00:00Z"), ts("2026-01-01T06:00:00Z")),
            "too late"
        );
    }

    #[test]
    fn scheduled_names_fit() {
        let name = scheduled_name(
            &"a".repeat(cubeharbor_core::backup::MAX_SCHEDULE_NAME),
            ts("2286-01-01T00:00:00Z"),
        );
        assert!(
            name.len() <= cubeharbor_core::backup::MAX_BACKUP_NAME,
            "{name}"
        );
        assert!(
            job_name(
                &"a".repeat(cubeharbor_core::backup::MAX_BACKUP_NAME),
                JobAction::Restore
            )
            .len()
                <= 63
        );
    }

    #[test]
    fn new_servers_start_locked_and_are_validated() {
        let restore = |raw| {
            MinecraftRestore::new(
                "r",
                MinecraftRestoreSpec {
                    target: RestoreTarget {
                        server: "fresh".into(),
                        overwrite_existing: false,
                        new_server: raw,
                    },
                    ..Default::default()
                },
            )
        };
        let s = new_server(
            &restore(Some(json!({ "version": "1.21.4" }))),
            "fresh",
            "mc",
            "stop:MinecraftRestore/r",
        )
        .unwrap();
        assert!(cubeharbor_core::backup::stop_requested(s.annotations()));
        assert_eq!(s.spec.version.as_deref(), Some("1.21.4"));
        assert!(
            new_server(&restore(None), "fresh", "mc", "x")
                .unwrap_err()
                .contains("newServer")
        );
        assert!(new_server(&restore(Some(json!({ "version": 5 }))), "fresh", "mc", "x").is_err());
        assert!(new_server(&restore(Some(json!({}))), "Bad_Name", "mc", "x").is_err());
    }

    #[test]
    fn status_patch_clears_the_message() {
        let v = status_patch(&MinecraftBackupStatus::default(), None).unwrap();
        assert!(v["status"].as_object().unwrap()["message"].is_null());
    }

    #[test]
    fn pod_states() {
        assert!(matches!(pod_state(None), ServerState::Stopped));
        let pod = |ready: &str| -> Pod {
            serde_json::from_value(json!({ "metadata": {}, "status": { "conditions": [{ "type": "Ready", "status": ready }] } })).unwrap()
        };
        assert!(matches!(pod_state(Some(&pod("True"))), ServerState::Ready));
        assert!(matches!(pod_state(Some(&pod("False"))), ServerState::Busy));
    }

    #[test]
    fn job_outcomes() {
        let job = |status| -> Job {
            serde_json::from_value(json!({ "metadata": {}, "status": status })).unwrap()
        };
        assert_eq!(job_finished(&job(json!({ "active": 1 }))), None);
        assert_eq!(job_finished(&job(json!({ "succeeded": 1 }))), Some(true));
        assert_eq!(job_finished(&job(json!({ "failed": 1 }))), Some(false));
        assert_eq!(
            job_finished(&job(
                json!({ "conditions": [{ "type": "Failed", "status": "True" }] })
            )),
            Some(false)
        );
    }
}
