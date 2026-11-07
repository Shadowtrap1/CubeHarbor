//! Leader election with a `coordination.k8s.io/v1` Lease, so several operator replicas can run
//! and only one reconciles (port allocation is coordinated inside that one process).
//!
//! The leader renews the Lease every `RETRY`. Another replica takes over once it has seen no
//! change to the Lease for `leaseDurationSeconds`, timed on its *own* monotonic clock: like
//! client-go, timestamps written by another node are never compared with the local clock, so
//! clock skew between nodes can't make a standby take over a live Lease. A leader that can't
//! renew within `RENEW_DEADLINE` (shorter than the lease duration) exits, so two replicas never
//! reconcile at once; on a normal shutdown it releases the Lease so a standby takes over right
//! away.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{Api, ObjectMeta, PostParams};
use kube::Client;

pub const LEASE_DURATION: Duration = Duration::from_secs(15);
pub const RENEW_DEADLINE: Duration = Duration::from_secs(10);
pub const RETRY: Duration = Duration::from_secs(2);

pub struct Elector {
    api: Api<Lease>,
    name: String,
    identity: String,
    observed: Mutex<Observer>,
}

/// When this replica last saw the Lease record (holder, renew time) change, on its own clock.
#[derive(Default)]
struct Observer {
    last: Option<(Option<String>, Option<jiff::Timestamp>, Instant)>,
}

impl Observer {
    /// How long the Lease has gone unchanged, as observed locally. A record seen for the first
    /// time counts as just changed, so a new standby waits a full lease duration.
    fn unchanged_for(&mut self, spec: Option<&LeaseSpec>, now: Instant) -> Duration {
        let holder = spec.and_then(|s| s.holder_identity.clone());
        let renewed = spec.and_then(|s| s.renew_time.as_ref().map(|t| t.0));
        match &self.last {
            Some((h, r, at)) if *h == holder && *r == renewed => now.saturating_duration_since(*at),
            _ => {
                self.last = Some((holder, renewed, now));
                Duration::ZERO
            }
        }
    }
}

/// What to do with the Lease as it is now.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Someone else holds a live lease.
    Wait,
    /// We hold it: renew.
    Renew,
    /// Free or expired: take it (a new term).
    Acquire,
}

/// `unchanged_for` is how long this replica has seen the Lease record unchanged (see [`Observer`]).
fn decide(spec: Option<&LeaseSpec>, identity: &str, unchanged_for: Duration) -> Decision {
    let Some(spec) = spec else { return Decision::Acquire };
    let holder = spec.holder_identity.as_deref().unwrap_or("");
    if holder == identity {
        return Decision::Renew;
    }
    if holder.is_empty() {
        return Decision::Acquire;
    }
    let duration = Duration::from_secs(spec.lease_duration_seconds.unwrap_or(LEASE_DURATION.as_secs() as i32).max(1) as u64);
    if unchanged_for >= duration { Decision::Acquire } else { Decision::Wait }
}

impl Elector {
    pub fn new(client: Client, namespace: &str, name: &str, identity: String) -> Self {
        Elector { api: Api::namespaced(client, namespace), name: name.to_string(), identity, observed: Mutex::default() }
    }

    /// One attempt to acquire or renew. `Ok(true)` = we are the leader now.
    pub async fn try_acquire_or_renew(&self) -> Result<bool> {
        let now = jiff::Timestamp::now();
        let existing = self.api.get_opt(&self.name).await.context("reading the leader Lease")?;
        let Some(mut lease) = existing else {
            let lease = Lease {
                metadata: ObjectMeta { name: Some(self.name.clone()), ..Default::default() },
                spec: Some(self.term(now, 0)),
            };
            return match self.api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                // Another replica created it first.
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
                Err(e) => Err(e).context("creating the leader Lease"),
            };
        };
        let transitions = lease.spec.as_ref().and_then(|s| s.lease_transitions).unwrap_or(0);
        let unchanged_for = self.observed.lock().unwrap_or_else(|e| e.into_inner()).unchanged_for(lease.spec.as_ref(), Instant::now());
        let spec = match decide(lease.spec.as_ref(), &self.identity, unchanged_for) {
            Decision::Wait => return Ok(false),
            Decision::Renew => {
                let mut s = lease.spec.clone().unwrap_or_default();
                s.renew_time = Some(MicroTime(now));
                s.lease_duration_seconds = Some(LEASE_DURATION.as_secs() as i32);
                s
            }
            Decision::Acquire => self.term(now, transitions + 1),
        };
        lease.spec = Some(spec);
        // `replace` sends the resourceVersion we read: if another replica wrote in between, the
        // API server answers 409 and we simply didn't win this round.
        match self.api.replace(&self.name, &PostParams::default(), &lease).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e).context("updating the leader Lease"),
        }
    }

    fn term(&self, now: jiff::Timestamp, transitions: i32) -> LeaseSpec {
        LeaseSpec {
            holder_identity: Some(self.identity.clone()),
            lease_duration_seconds: Some(LEASE_DURATION.as_secs() as i32),
            acquire_time: Some(MicroTime(now)),
            renew_time: Some(MicroTime(now)),
            lease_transitions: Some(transitions),
            ..Default::default()
        }
    }

    /// Block until this replica is the leader.
    pub async fn acquire(&self) {
        let mut logged_wait = false;
        loop {
            match self.try_acquire_or_renew().await {
                Ok(true) => return,
                Ok(false) if !logged_wait => {
                    tracing::info!("another replica is the leader; standing by");
                    logged_wait = true;
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("leader election: {e:#}"),
            }
            tokio::time::sleep(RETRY).await;
        }
    }

    /// Renew every `RETRY` until a renewal fails for longer than `RENEW_DEADLINE` or another
    /// replica took the Lease. Returns when leadership is lost.
    pub async fn hold(&self) {
        let mut last_ok = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(RETRY).await;
            match self.try_acquire_or_renew().await {
                Ok(true) => last_ok = tokio::time::Instant::now(),
                // Someone else wrote the Lease since we read it (another replica, or e.g. a
                // `kubectl annotate`). Exiting is conservative: a fresh process re-joins the election.
                Ok(false) => {
                    tracing::error!("leader election: another replica took the Lease");
                    return;
                }
                Err(e) => {
                    tracing::warn!("leader election: renewing: {e:#}");
                    if last_ok.elapsed() > RENEW_DEADLINE {
                        tracing::error!("leader election: could not renew the Lease for {}s", RENEW_DEADLINE.as_secs());
                        return;
                    }
                }
            }
        }
    }

    /// Give the Lease up (on shutdown), so a standby doesn't wait for it to expire.
    pub async fn release(&self) {
        let Ok(Some(mut lease)) = self.api.get_opt(&self.name).await else { return };
        let Some(spec) = lease.spec.as_mut() else { return };
        if spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
            return;
        }
        spec.holder_identity = None;
        spec.lease_duration_seconds = Some(1);
        match self.api.replace(&self.name, &PostParams::default(), &lease).await {
            Ok(_) => tracing::info!("released the leader Lease"),
            Err(e) => tracing::warn!("releasing the leader Lease: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(holder: Option<&str>, renewed: i64, duration: i32) -> LeaseSpec {
        LeaseSpec {
            holder_identity: holder.map(String::from),
            renew_time: Some(MicroTime(jiff::Timestamp::from_second(renewed).unwrap())),
            lease_duration_seconds: Some(duration),
            ..Default::default()
        }
    }

    #[test]
    fn decisions() {
        let s = Duration::from_secs;
        assert_eq!(decide(None, "me", s(0)), Decision::Acquire, "no lease yet");
        assert_eq!(decide(Some(&spec(Some("me"), 990, 15)), "me", s(0)), Decision::Renew);
        assert_eq!(decide(Some(&spec(Some("other"), 990, 15)), "me", s(14)), Decision::Wait, "live lease");
        assert_eq!(decide(Some(&spec(Some("other"), 990, 15)), "me", s(15)), Decision::Acquire, "expired");
        assert_eq!(decide(Some(&spec(None, 999, 15)), "me", s(0)), Decision::Acquire, "released");
        assert_eq!(decide(Some(&spec(Some(""), 999, 15)), "me", s(0)), Decision::Acquire, "released");
    }

    /// Expiry is timed locally from the last observed change, whatever the renew timestamps say
    /// (they come from another node's clock).
    #[test]
    fn expiry_uses_the_local_clock() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut o = Observer::default();
        // A renewTime far in the past (the leader's clock is behind ours) is not "expired" on sight.
        let old_looking = spec(Some("other"), 0, 15);
        assert_eq!(o.unchanged_for(Some(&old_looking), t0), s(0));
        assert_eq!(decide(Some(&old_looking), "me", o.unchanged_for(Some(&old_looking), t0 + s(10))), Decision::Wait);
        // The leader renews: the local clock restarts, although the new renewTime still looks old.
        let renewed = spec(Some("other"), 2, 15);
        assert_eq!(o.unchanged_for(Some(&renewed), t0 + s(12)), s(0));
        assert_eq!(o.unchanged_for(Some(&renewed), t0 + s(26)), s(14));
        // No change for a full lease duration: take over.
        let waited = o.unchanged_for(Some(&renewed), t0 + s(27));
        assert_eq!(decide(Some(&renewed), "me", waited), Decision::Acquire);
        // A different holder counts as a change too.
        assert_eq!(o.unchanged_for(Some(&spec(Some("third"), 2, 15)), t0 + s(28)), s(0));
    }
}
