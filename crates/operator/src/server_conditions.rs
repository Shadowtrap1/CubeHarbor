//! `MinecraftServer` conditions, built on the generic [`crate::conditions`]. The types are:
//!
//! * `Reconciled`: the spec was accepted and applied (`False` with reason `InvalidSpec` for a spec
//!   the operator refuses, or `ReconcileFailed` for an error it retries).
//! * `Installed`: the install init container finished for the current pod.
//! * `Routable`: the gateway has a hostname (or the default route) for this server.
//! * `Ready`: the Minecraft server is up, so `kubectl wait --for=condition=Ready` works.

use cubeharbor_core::crd::{MinecraftServerStatus, Phase};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

use crate::conditions::{Policy, Transition, Verdict, get, set};

pub const READY: &str = "Ready";
pub const INSTALLED: &str = "Installed";
pub const ROUTABLE: &str = "Routable";
pub const RECONCILED: &str = "Reconciled";

pub const REASON_INVALID_SPEC: &str = "InvalidSpec";
pub const REASON_RECONCILE_FAILED: &str = "ReconcileFailed";
const REASON_ROUTING_CONFLICT: &str = "RoutingConflict";
const REASON_INSTALL_FAILED: &str = "InstallFailed";
const REASON_SERVER_ERROR: &str = "ServerError";
const REASON_INSTALLING: &str = "Installing";
const REASON_PENDING: &str = "Pending";

const POLICY: Policy = Policy {
    warning_reasons: &[REASON_INVALID_SPEC, REASON_RECONCILE_FAILED, REASON_ROUTING_CONFLICT, REASON_INSTALL_FAILED, REASON_SERVER_ERROR],
    is_noise,
};

/// Events for what changed between two condition lists.
pub fn transitions(previous: &[Condition], current: &[Condition]) -> Vec<Transition> {
    crate::conditions::transitions(previous, current, &POLICY)
}

/// Progress steps that would only repeat what another condition already reports.
fn is_noise(c: &Condition, before: Option<&Condition>) -> bool {
    match c.type_.as_str() {
        INSTALLED => c.status != "True" && c.reason != REASON_INSTALL_FAILED,
        READY => c.reason == REASON_PENDING && before.is_some_and(|p| p.status != "True"),
        _ => false,
    }
}

/// How far the install init container got on the current pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    Done,
    InProgress,
    Failed,
    /// No pod (asleep, paused, not created yet), or it hasn't reported init containers.
    Unknown,
}

pub fn install_state(pod: Option<&Pod>) -> Install {
    let Some(pod) = pod.filter(|p| p.metadata.deletion_timestamp.is_none()) else { return Install::Unknown };
    let statuses = pod.status.iter().flat_map(|s| s.init_container_statuses.iter().flatten());
    let mut seen = false;
    for cs in statuses {
        seen = true;
        let state = cs.state.as_ref();
        let crashing = state.and_then(|s| s.waiting.as_ref()).is_some_and(|w| w.reason.as_deref() == Some("CrashLoopBackOff"));
        match state.and_then(|s| s.terminated.as_ref()) {
            Some(t) if t.exit_code != 0 => return Install::Failed,
            Some(_) => {}
            None if crashing => return Install::Failed,
            None => return Install::InProgress,
        }
    }
    if seen { Install::Done } else { Install::Unknown }
}

fn ready_verdict(phase: Phase, message: Option<&str>) -> Verdict {
    let text = |default: &str| message.unwrap_or(default).to_string();
    match phase {
        Phase::Running => ("True", "Running", text("server is running")),
        Phase::Sleeping => ("False", "Sleeping", text("sleeping until a player joins")),
        Phase::Stopped => ("False", "Paused", text("spec.paused is set")),
        Phase::Installing => ("False", REASON_INSTALLING, text("installing server and mods")),
        Phase::Starting => ("False", "Starting", text("server is starting")),
        Phase::Pending => ("False", REASON_PENDING, text("waiting for pod")),
        Phase::Error => ("False", REASON_SERVER_ERROR, text("server failed")),
    }
}

fn routable_verdict(hostnames: &[String], is_default: bool, conflict: Option<&str>) -> Verdict {
    let reachable = !hostnames.is_empty() || is_default;
    match (conflict, reachable) {
        (Some(c), reachable) => (if reachable { "True" } else { "False" }, REASON_ROUTING_CONFLICT, c.to_string()),
        (None, false) => ("False", "NoHostnames", "no hostname: set spec.routing.hostnames or the operator's base domain".to_string()),
        (None, true) if hostnames.is_empty() => ("True", "DefaultServer", "default server for unknown hostnames".to_string()),
        (None, true) => ("True", "Routed", format!("routed from {}", hostnames.join(", "))),
    }
}

/// What a successful reconcile observed.
pub struct Observed<'a> {
    pub install: Install,
    pub is_default: bool,
    pub routing_conflict: Option<&'a str>,
}

/// Record a successful reconcile. Call with the final `phase` and with `message` not yet carrying
/// the routing conflict, which belongs to `Routable`.
pub fn on_success(status: &mut MinecraftServerStatus, generation: Option<i64>, now: &Time, observed: Observed) {
    let ready = ready_verdict(status.phase, status.message.as_deref());
    let routable = routable_verdict(&status.hostnames, observed.is_default, observed.routing_conflict);
    let installed = match observed.install {
        Install::Done => Some(("True", "InstallComplete", "install finished".to_string())),
        Install::InProgress => Some(("False", REASON_INSTALLING, "installing server and mods".to_string())),
        Install::Failed => Some(("False", REASON_INSTALL_FAILED, "install failed (see the install container logs)".to_string())),
        // The files stay on the volume while the pod is gone, so the last verdict still holds.
        Install::Unknown if get(&status.conditions, INSTALLED).is_some() => None,
        Install::Unknown => Some(("Unknown", "NoPod", "no server pod to inspect yet".to_string())),
    };
    let conditions = &mut status.conditions;
    set(conditions, RECONCILED, ("True", "ReconcileSucceeded", "spec applied".to_string()), generation, now);
    if let Some(v) = installed {
        set(conditions, INSTALLED, v, generation, now);
    }
    set(conditions, ROUTABLE, routable, generation, now);
    set(conditions, READY, ready, generation, now);
}

/// Record a failed reconcile; `status.phase` must already hold the phase to report. Nothing was
/// observed, so conditions that exist keep their value and their older `observedGeneration`,
/// unless the server is now in `Error`, where `Ready` has to say why.
pub fn on_failure(status: &mut MinecraftServerStatus, generation: Option<i64>, now: &Time, reason: &'static str, message: &str) {
    let phase = status.phase;
    let conditions = &mut status.conditions;
    set(conditions, RECONCILED, ("False", reason, message.to_string()), generation, now);
    if phase == Phase::Error {
        set(conditions, READY, ("False", reason, message.to_string()), generation, now);
    } else if get(conditions, READY).is_none() {
        set(conditions, READY, ready_verdict(phase, None), generation, now);
    }
    for type_ in [INSTALLED, ROUTABLE] {
        if get(conditions, type_).is_none() {
            set(conditions, type_, ("Unknown", "NotEvaluated", "not evaluated: the reconcile failed".to_string()), generation, now);
        }
    }
}

/// Whether the status has to be written. A player sample is left out of the comparison: servers
/// return a shuffled subset on every ping, and each write would trigger another reconcile. It
/// still goes out with the next real change (`lastActive` moves every minute with players on).
pub fn status_changed(old: Option<&MinecraftServerStatus>, new: &MinecraftServerStatus) -> bool {
    fn without_sample(s: &MinecraftServerStatus) -> MinecraftServerStatus {
        let mut s = s.clone();
        if let Some(p) = &mut s.players {
            p.sample.clear();
        }
        s
    }
    old.map(without_sample) != Some(without_sample(new))
}

/// True when `new` differs from `old` only in message texts (`status.message` and condition
/// messages). Failure messages can vary per attempt (addresses, counters, timings), and each
/// write comes back as a watch event that triggers the next attempt at once.
pub fn only_messages_differ(old: Option<&MinecraftServerStatus>, new: &MinecraftServerStatus) -> bool {
    let Some(old) = old else { return false };
    let mut n = new.clone();
    n.message = old.message.clone();
    for c in &mut n.conditions {
        if let Some(o) = get(&old.conditions, &c.type_) {
            c.message = o.message.clone();
        }
    }
    status_changed(Some(old), new) && !status_changed(Some(old), &n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubeharbor_core::crd::Players;
    use k8s_openapi::api::core::v1::{ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus, PodStatus};

    fn at(second: i64) -> Time {
        Time(jiff::Timestamp::from_second(second).unwrap())
    }

    #[test]
    fn a_failure_that_only_reworded_its_message_is_recognised() {
        let mut old = MinecraftServerStatus { phase: Phase::Error, ..Default::default() };
        on_failure(&mut old, Some(1), &at(0), REASON_RECONCILE_FAILED, "connect to 10.0.0.1:443 failed");
        old.message = Some("connect to 10.0.0.1:443 failed".into());
        let mut new = old.clone();
        on_failure(&mut new, Some(1), &at(60), REASON_RECONCILE_FAILED, "connect to 10.0.0.2:443 failed");
        new.message = Some("connect to 10.0.0.2:443 failed".into());
        assert!(only_messages_differ(Some(&old), &new));
        assert!(!only_messages_differ(Some(&old), &old));
        assert!(!only_messages_differ(None, &new));
        let mut other = new.clone();
        other.phase = Phase::Running;
        assert!(!only_messages_differ(Some(&old), &other));
    }

    fn observed() -> Observed<'static> {
        Observed { install: Install::Done, is_default: false, routing_conflict: None }
    }

    fn status() -> MinecraftServerStatus {
        MinecraftServerStatus { hostnames: vec!["a.test".into()], ..Default::default() }
    }

    fn cond<'a>(s: &'a MinecraftServerStatus, type_: &str) -> &'a Condition {
        get(&s.conditions, type_).unwrap_or_else(|| panic!("no {type_} condition"))
    }

    #[test]
    fn unchanged_reconcile_needs_no_write() {
        let mut first = status();
        first.phase = Phase::Running;
        on_success(&mut first, Some(3), &at(100), observed());

        // Same observations a minute later, starting from what was stored.
        let mut second = first.clone();
        on_success(&mut second, Some(3), &at(160), observed());
        assert_eq!(first, second);
        assert!(!status_changed(Some(&first), &second));

        // A repeated failure is just as quiet, and so is the stored JSON round trip.
        let mut failed = first.clone();
        on_failure(&mut failed, Some(3), &at(200), REASON_RECONCILE_FAILED, "boom");
        let stored: MinecraftServerStatus = serde_json::from_value(serde_json::to_value(&failed).unwrap()).unwrap();
        let mut again = stored.clone();
        on_failure(&mut again, Some(3), &at(260), REASON_RECONCILE_FAILED, "boom");
        assert!(!status_changed(Some(&stored), &again));
    }

    #[test]
    fn real_changes_are_written() {
        let old = status();
        assert!(status_changed(None, &old), "first status");
        assert!(status_changed(None, &MinecraftServerStatus::default()), "even an empty first status");

        let mut new = old.clone();
        new.phase = Phase::Running;
        assert!(status_changed(Some(&old), &new));

        let mut new = old.clone();
        new.observed_generation = Some(9);
        assert!(status_changed(Some(&old), &new));
    }

    #[test]
    fn a_new_player_sample_alone_is_not_a_change() {
        let players = |online, sample: &[&str]| Some(Players { online, max: 20, sample: sample.iter().map(|s| s.to_string()).collect() });
        let old = MinecraftServerStatus { players: players(2, &["a", "b"]), ..Default::default() };
        assert!(!status_changed(Some(&old), &MinecraftServerStatus { players: players(2, &["b", "a"]), ..Default::default() }));
        assert!(status_changed(Some(&old), &MinecraftServerStatus { players: players(3, &["a", "b"]), ..Default::default() }));
    }

    #[test]
    fn ready_follows_the_phase() {
        for (phase, want, reason) in [
            (Phase::Running, "True", "Running"),
            (Phase::Sleeping, "False", "Sleeping"),
            (Phase::Stopped, "False", "Paused"),
            (Phase::Installing, "False", "Installing"),
            (Phase::Starting, "False", "Starting"),
            (Phase::Pending, "False", "Pending"),
            (Phase::Error, "False", "ServerError"),
        ] {
            let mut s = status();
            s.phase = phase;
            on_success(&mut s, Some(1), &at(1), observed());
            let c = cond(&s, READY);
            assert_eq!((c.status.as_str(), c.reason.as_str()), (want, reason), "{phase:?}");
            assert_eq!(cond(&s, RECONCILED).status, "True");
        }
    }

    #[test]
    fn routing_conflicts_and_missing_hostnames() {
        let check = |hosts: &[&str], default, conflict, want: (&str, &str)| {
            let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
            let (status, reason, _) = routable_verdict(&hosts, default, conflict);
            assert_eq!((status, reason), want);
        };
        check(&["a.test"], false, None, ("True", "Routed"));
        check(&[], true, None, ("True", "DefaultServer"));
        check(&[], false, None, ("False", "NoHostnames"));
        check(&["b.test"], false, Some("x"), ("True", "RoutingConflict"));
        check(&[], false, Some("x"), ("False", "RoutingConflict"));
    }

    #[test]
    fn a_failure_keeps_what_it_did_not_observe() {
        let mut s = status();
        s.phase = Phase::Running;
        on_success(&mut s, Some(1), &at(10), observed());
        let ready_before = cond(&s, READY).clone();

        on_failure(&mut s, Some(2), &at(20), REASON_INVALID_SPEC, "bad port");
        let reconciled = cond(&s, RECONCILED);
        assert_eq!((reconciled.status.as_str(), reconciled.reason.as_str(), reconciled.message.as_str()), ("False", "InvalidSpec", "bad port"));
        assert_eq!(reconciled.observed_generation, Some(2));
        // The server is still up: Ready stands, marked as evaluated for the older generation.
        assert_eq!(cond(&s, READY), &ready_before);

        on_success(&mut s, Some(2), &at(30), observed());
        assert_eq!(cond(&s, RECONCILED).status, "True");
        assert_eq!(cond(&s, RECONCILED).last_transition_time, at(30));
        assert_eq!(cond(&s, READY).last_transition_time, at(10), "Ready never changed");
    }

    #[test]
    fn a_rejected_new_server_reports_every_condition() {
        let mut s = status();
        s.phase = Phase::Error;
        on_failure(&mut s, Some(1), &at(10), REASON_INVALID_SPEC, "bad port");
        assert_eq!(cond(&s, READY).reason, "InvalidSpec");
        assert_eq!(cond(&s, INSTALLED).status, "Unknown");
        assert_eq!(cond(&s, ROUTABLE).status, "Unknown");

        // Two conditions fail for the same reason, but it is one Event.
        let events = transitions(&[], &s.conditions);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].warning);
        assert_eq!(events[0].reason, "InvalidSpec");
        assert!(events[0].note.contains("bad port"));
    }

    #[test]
    fn events_fire_once_per_transition() {
        let mut s = status();
        s.phase = Phase::Starting;
        on_success(&mut s, Some(1), &at(1), observed());
        assert!(transitions(&[], &s.conditions).is_empty(), "first evaluation of a healthy server is silent");

        let before = s.conditions.clone();
        s.phase = Phase::Running;
        on_success(&mut s, Some(1), &at(2), observed());
        let events = transitions(&before, &s.conditions);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!((events[0].warning, events[0].reason.as_str()), (false, "Running"));
        assert!(transitions(&s.conditions, &s.conditions).is_empty());

        // A repeating failure is reported once, even when its text changes.
        let before = s.conditions.clone();
        on_failure(&mut s, Some(1), &at(3), REASON_RECONCILE_FAILED, "boom");
        assert_eq!(transitions(&before, &s.conditions).len(), 1);
        let before = s.conditions.clone();
        on_failure(&mut s, Some(1), &at(4), REASON_RECONCILE_FAILED, "boom, differently");
        assert!(transitions(&before, &s.conditions).is_empty());

        // Recovery is an Event of its own.
        let before = s.conditions.clone();
        on_success(&mut s, Some(1), &at(5), observed());
        let events = transitions(&before, &s.conditions);
        assert_eq!(events.iter().map(|e| (e.warning, e.reason.as_str())).collect::<Vec<_>>(), [(false, "ReconcileSucceeded")]);
    }

    #[test]
    fn install_progress_is_not_an_event_but_its_failure_is() {
        let mut s = status();
        s.phase = Phase::Installing;
        on_success(&mut s, Some(1), &at(1), Observed { install: Install::InProgress, ..observed() });
        let before = s.conditions.clone();
        on_success(&mut s, Some(1), &at(2), Observed { install: Install::Failed, ..observed() });
        let events = transitions(&before, &s.conditions);
        assert_eq!(events.iter().map(|e| (e.warning, e.reason.as_str())).collect::<Vec<_>>(), [(true, "InstallFailed")]);
        assert_eq!(cond(&s, INSTALLED).status, "False");
    }

    #[test]
    fn installed_survives_the_pod_going_away() {
        let mut s = status();
        s.phase = Phase::Running;
        on_success(&mut s, Some(1), &at(1), observed());
        s.phase = Phase::Sleeping;
        on_success(&mut s, Some(1), &at(2), Observed { install: Install::Unknown, ..observed() });
        assert_eq!((cond(&s, INSTALLED).status.as_str(), &cond(&s, INSTALLED).last_transition_time), ("True", &at(1)));

        let mut fresh = status();
        on_success(&mut fresh, Some(1), &at(2), Observed { install: Install::Unknown, ..observed() });
        assert_eq!(cond(&fresh, INSTALLED).status, "Unknown");
    }

    fn pod(states: Vec<ContainerState>) -> Pod {
        let statuses = states.into_iter().map(|state| ContainerStatus { state: Some(state), ..Default::default() }).collect();
        Pod { status: Some(PodStatus { init_container_statuses: Some(statuses), ..Default::default() }), ..Default::default() }
    }

    #[test]
    fn install_state_reads_the_init_containers() {
        let exited = |exit_code| ContainerState { terminated: Some(ContainerStateTerminated { exit_code, ..Default::default() }), ..Default::default() };
        let running = ContainerState { running: Some(ContainerStateRunning::default()), ..Default::default() };
        let waiting = |reason: &str| ContainerState {
            waiting: Some(ContainerStateWaiting { reason: Some(reason.into()), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(install_state(None), Install::Unknown);
        assert_eq!(install_state(Some(&Pod::default())), Install::Unknown);
        assert_eq!(install_state(Some(&pod(vec![exited(0)]))), Install::Done);
        assert_eq!(install_state(Some(&pod(vec![exited(1)]))), Install::Failed);
        assert_eq!(install_state(Some(&pod(vec![running]))), Install::InProgress);
        assert_eq!(install_state(Some(&pod(vec![waiting("PodInitializing")]))), Install::InProgress);
        assert_eq!(install_state(Some(&pod(vec![waiting("CrashLoopBackOff")]))), Install::Failed);
    }
}
