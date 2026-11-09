//! Public port allocation for voice chat, Bedrock and other extra ports.
//!
//! Allocations live in each server's status. A process-wide lock serialises allocation so two
//! servers reconciled concurrently can't grab the same port.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};
use cubeharbor_core::MinecraftServer;
use cubeharbor_core::crd::{PortAllocation, PortRequest};
use kube::ResourceExt;

/// Ports in use by *other* servers, keyed by (port, protocol) -> owner.
pub type Used = HashMap<(u16, cubeharbor_core::crd::Protocol), String>;

/// Allocations this process wrote that the watch cache may not show yet, by server UID. Reading
/// the cache instead of LISTing per reconcile would otherwise let two servers reconciled in a row
/// take the same port. Only the leader allocates, so nothing else can make an entry wrong.
pub type Ledger = HashMap<String, Vec<PortAllocation>>;

/// A server's allocations: what was just written, else what the cache has.
pub fn allocations_of<'a>(ledger: &'a Ledger, server: &'a MinecraftServer) -> &'a [PortAllocation] {
    let cached = || server.status.as_ref().map(|s| s.allocations.as_slice()).unwrap_or_default();
    server.uid().and_then(|uid| ledger.get(&uid)).map_or_else(cached, Vec::as_slice)
}

/// Forget entries the cache has caught up with, and those of servers that are gone.
pub fn settle(ledger: &mut Ledger, servers: &[std::sync::Arc<MinecraftServer>]) {
    ledger.retain(|uid, written| {
        servers.iter().any(|s| s.uid().as_deref() == Some(uid.as_str()) && s.status.as_ref().map(|st| &st.allocations) != Some(&*written))
    });
}

/// Allocate ports for `requests`, keeping existing allocations when they still match.
pub fn allocate(
    owner: &str,
    requests: &[PortRequest],
    current: &[PortAllocation],
    used: &Used,
    range: (u16, u16),
) -> Result<Vec<PortAllocation>> {
    let mut ports: Vec<Option<u16>> = vec![None; requests.len()];
    let mut taken: BTreeSet<(u16, cubeharbor_core::crd::Protocol)> = BTreeSet::new();
    // Pass 1: pinned ports. Two pins on the same port are a real conflict.
    for (i, req) in requests.iter().enumerate() {
        let Some(pinned) = req.public_port else { continue };
        if let Some(other) = used.get(&(pinned, req.protocol)) {
            bail!("public port {pinned}/{} for `{}` is already used by {other}", req.protocol.as_str(), req.name);
        }
        if !taken.insert((pinned, req.protocol)) {
            bail!("port {pinned}/{} requested twice by {owner}", req.protocol.as_str());
        }
        ports[i] = Some(pinned);
    }
    // Pass 2: keep existing ports, unless a pin now claims them (then the request moves).
    for (i, req) in requests.iter().enumerate() {
        if ports[i].is_some() {
            continue;
        }
        if let Some(a) = current.iter().find(|a| {
            a.name == req.name && a.protocol == req.protocol && !used.contains_key(&(a.public_port, req.protocol))
        }) && taken.insert((a.public_port, req.protocol))
        {
            ports[i] = Some(a.public_port);
        }
    }
    // Pass 3: everything else gets the lowest free port.
    let mut out = vec![];
    for (req, port) in requests.iter().zip(ports) {
        let port = match port {
            Some(p) => p,
            None => {
                let free = (range.0..=range.1)
                    .find(|p| !used.contains_key(&(*p, req.protocol)) && !taken.contains(&(*p, req.protocol)));
                let Some(p) = free else { bail!("no free {} ports left in {}-{}", req.protocol.as_str(), range.0, range.1) };
                taken.insert((p, req.protocol));
                p
            }
        };
        out.push(PortAllocation { name: req.name.clone(), protocol: req.protocol, container_port: req.container_port, public_port: port });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cubeharbor_core::crd::Protocol::*;

    fn req(name: &str, pinned: Option<u16>) -> PortRequest {
        PortRequest { name: name.into(), protocol: Udp, container_port: 24454, public_port: pinned }
    }

    #[test]
    fn the_ledger_overrides_the_cache_until_it_catches_up() {
        use cubeharbor_core::crd::{MinecraftServerSpec, MinecraftServerStatus};
        use std::sync::Arc;
        let alloc = |port| PortAllocation { name: "voice".into(), protocol: Udp, container_port: 24454, public_port: port };
        let server = |uid: &str, port: Option<u16>| {
            let mut s = MinecraftServer::new("a", MinecraftServerSpec::default());
            s.metadata.uid = Some(uid.into());
            s.status = port.map(|p| MinecraftServerStatus { allocations: vec![alloc(p)], ..Default::default() });
            Arc::new(s)
        };
        let mut ledger = Ledger::from([("u1".to_string(), vec![alloc(25600)])]);

        // The cache is behind: the entry wins and stays.
        let behind = vec![server("u1", None)];
        assert_eq!(allocations_of(&ledger, &behind[0]), [alloc(25600)]);
        settle(&mut ledger, &behind);
        assert_eq!(ledger.len(), 1);

        // Caught up: nothing left to remember.
        settle(&mut ledger, &[server("u1", Some(25600))]);
        assert!(ledger.is_empty());

        // A deleted server's ports are free again.
        let mut ledger = Ledger::from([("gone".to_string(), vec![alloc(25600)])]);
        settle(&mut ledger, &[server("u1", None)]);
        assert!(ledger.is_empty());
    }

    #[test]
    fn allocates_lowest_free_and_is_sticky() {
        let mut used = Used::new();
        used.insert((100, Udp), "ns/other".into());
        let a = allocate("me", &[req("voice", None), req("bedrock", None)], &[], &used, (100, 110)).unwrap();
        assert_eq!(a.iter().map(|a| a.public_port).collect::<Vec<_>>(), vec![101, 102]);
        // Re-running keeps the same ports even if lower ones free up.
        let again = allocate("me", &[req("voice", None)], &a, &Used::new(), (100, 110)).unwrap();
        assert_eq!(again[0].public_port, 101);
    }

    #[test]
    fn pinned_conflicts_fail() {
        let mut used = Used::new();
        used.insert((105, Udp), "ns/other".into());
        assert!(allocate("me", &[req("voice", Some(105))], &[], &used, (100, 110)).is_err());
        assert_eq!(allocate("me", &[req("voice", Some(105))], &[], &Used::new(), (100, 110)).unwrap()[0].public_port, 105);
    }

    /// Adding voice chat (listed first) to a server that already has Bedrock must not steal
    /// Bedrock's port and fail with "requested twice".
    #[test]
    fn new_request_does_not_take_existing_port() {
        let current = allocate("me", &[req("bedrock", None)], &[], &Used::new(), (100, 110)).unwrap();
        assert_eq!(current[0].public_port, 100);
        let a = allocate("me", &[req("voice", None), req("bedrock", None)], &current, &Used::new(), (100, 110)).unwrap();
        assert_eq!(a.iter().map(|a| (a.name.as_str(), a.public_port)).collect::<Vec<_>>(), vec![("voice", 101), ("bedrock", 100)]);
    }

    #[test]
    fn new_request_does_not_take_pinned_port() {
        let a = allocate("me", &[req("voice", None), req("bedrock", Some(100))], &[], &Used::new(), (100, 110)).unwrap();
        assert_eq!(a.iter().map(|a| a.public_port).collect::<Vec<_>>(), vec![101, 100]);
    }

    /// Pinning another request to a port this server already uses moves the old holder.
    #[test]
    fn pin_displaces_sticky_port() {
        let current = allocate("me", &[req("voice", None)], &[], &Used::new(), (100, 110)).unwrap();
        assert_eq!(current[0].public_port, 100);
        let a = allocate("me", &[req("voice", None), req("bedrock", Some(100))], &current, &Used::new(), (100, 110)).unwrap();
        assert_eq!(a.iter().map(|a| (a.name.as_str(), a.public_port)).collect::<Vec<_>>(), vec![("voice", 101), ("bedrock", 100)]);
        assert!(allocate("me", &[req("voice", Some(105)), req("bedrock", Some(105))], &[], &Used::new(), (100, 110)).is_err());
    }

    #[test]
    fn exhaustion() {
        assert!(allocate("me", &[req("a", None), req("b", None)], &[], &Used::new(), (100, 100)).is_err());
    }
}
