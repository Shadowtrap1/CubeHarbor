//! `metav1.Condition` bookkeeping and Events for any custom resource this operator manages.
//!
//! A resource supplies its condition types and a [`Policy`]; `set`, `transitions` and `publish`
//! do the rest. Everything except `publish` is pure. See `server_conditions` for an example.

use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::runtime::events::{Event, EventType, Recorder};

/// Limit of an Event's `note`, also applied to condition messages so an enormous error can't
/// bloat the object.
const MAX_MESSAGE_BYTES: usize = 1024;

/// (`status`, `reason`, `message`) of a condition.
pub type Verdict = (&'static str, &'static str, String);

/// The current time at the second precision the API server stores.
pub fn now() -> Time {
    let ts = jiff::Timestamp::now();
    Time(jiff::Timestamp::from_second(ts.as_second()).unwrap_or(ts))
}

fn truncate(message: &str) -> String {
    let mut end = message.len().min(MAX_MESSAGE_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

/// Insert or update one condition. `lastTransitionTime` only moves when `status` changes, so
/// re-evaluating an unchanged condition leaves it byte-identical.
pub fn set(conditions: &mut Vec<Condition>, type_: &str, (status, reason, message): Verdict, generation: Option<i64>, now: &Time) {
    let message = truncate(&message);
    match conditions.iter_mut().find(|c| c.type_ == type_) {
        Some(c) => {
            if c.status != status {
                c.status = status.to_string();
                c.last_transition_time = now.clone();
            }
            c.reason = reason.to_string();
            c.message = message;
            c.observed_generation = generation;
        }
        None => conditions.push(Condition {
            type_: type_.to_string(),
            status: status.to_string(),
            reason: reason.to_string(),
            message,
            observed_generation: generation,
            last_transition_time: now.clone(),
        }),
    }
}

pub fn get<'a>(conditions: &'a [Condition], type_: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == type_)
}

/// How a resource's condition changes become Events.
pub struct Policy {
    /// Reasons reported as Warning; every other transition is Normal.
    pub warning_reasons: &'static [&'static str],
    /// Changes not worth an Event: (new condition, the condition before, if any).
    pub is_noise: fn(&Condition, Option<&Condition>) -> bool,
}

/// An Event to publish for a condition transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub warning: bool,
    pub reason: String,
    pub note: String,
}

/// Events for what changed between two condition lists. Only a change of status or reason counts,
/// so a failure that keeps repeating (even with a new message) reports once. Conditions appearing
/// for the first time are silent unless they are a problem, so upgrading the operator doesn't
/// emit an event for every healthy object.
pub fn transitions(previous: &[Condition], current: &[Condition], policy: &Policy) -> Vec<Transition> {
    let mut out: Vec<(Transition, &str)> = vec![];
    for c in current {
        let warning = policy.warning_reasons.contains(&c.reason.as_str());
        let before = get(previous, &c.type_);
        let changed = before.is_none_or(|p| p.status != c.status || p.reason != c.reason);
        if !changed || !(warning || before.is_some()) || (policy.is_noise)(c, before) {
            continue;
        }
        // Two conditions can fail for the same reason; one Event says it.
        if !out.iter().any(|(t, message)| t.reason == c.reason && *message == c.message) {
            let note = truncate(&format!("{}={}: {}", c.type_, c.status, c.message));
            out.push((Transition { warning, reason: c.reason.clone(), note }, c.message.as_str()));
        }
    }
    out.into_iter().map(|(t, _)| t).collect()
}

/// Publish transitions on `reference`. Call after the status is stored, so a failed write doesn't
/// report what the next attempt reports again. Best effort: a lost Event must not fail a reconcile.
pub async fn publish(recorder: &Recorder, reference: &ObjectReference, transitions: &[Transition]) {
    for t in transitions {
        let event = Event {
            type_: if t.warning { EventType::Warning } else { EventType::Normal },
            reason: t.reason.clone(),
            note: Some(t.note.clone()),
            action: "Reconcile".into(),
            secondary: None,
        };
        if let Err(e) = recorder.publish(&event, reference).await {
            tracing::warn!("{}/{}: publishing event {}: {e}", reference.namespace.as_deref().unwrap_or_default(), reference.name.as_deref().unwrap_or_default(), t.reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(second: i64) -> Time {
        Time(jiff::Timestamp::from_second(second).unwrap())
    }

    fn v(status: &'static str, reason: &'static str) -> Verdict {
        (status, reason, "m".to_string())
    }

    #[test]
    fn transition_time_moves_only_when_status_changes() {
        let mut c = vec![];
        set(&mut c, "Ready", v("False", "Starting"), Some(1), &at(10));
        assert_eq!(c[0].last_transition_time, at(10));

        // Same status, new reason and generation: the time stays.
        set(&mut c, "Ready", v("False", "Installing"), Some(2), &at(20));
        assert_eq!((c[0].reason.as_str(), c[0].observed_generation, &c[0].last_transition_time), ("Installing", Some(2), &at(10)));

        set(&mut c, "Ready", v("True", "Running"), Some(2), &at(30));
        assert_eq!(c[0].last_transition_time, at(30));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn long_messages_are_cut_on_a_character_boundary() {
        let mut c = vec![];
        set(&mut c, "Ready", ("False", "X", "é".repeat(1000)), None, &at(1));
        assert!(c[0].message.len() <= MAX_MESSAGE_BYTES);
        assert!(c[0].message.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn the_policy_decides_severity_and_noise() {
        let policy = Policy { warning_reasons: &["Bad"], is_noise: |c, _| c.reason == "Dull" };
        let mut before = vec![];
        set(&mut before, "A", v("True", "Fine"), None, &at(1));
        let mut after = before.clone();
        set(&mut after, "A", v("False", "Bad"), None, &at(2));
        let events = transitions(&before, &after, &policy);
        assert_eq!(events.iter().map(|e| (e.warning, e.reason.as_str())).collect::<Vec<_>>(), [(true, "Bad")]);

        let mut dull = before.clone();
        set(&mut dull, "A", v("False", "Dull"), None, &at(2));
        assert!(transitions(&before, &dull, &policy).is_empty());
    }
}
