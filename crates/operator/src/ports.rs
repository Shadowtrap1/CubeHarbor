//! Public port allocation for voice chat, Bedrock and other extra ports.
//!
//! Allocations live in each server's status. A process-wide lock serialises allocation so two
//! servers reconciled concurrently can't grab the same port.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};
use cubeharbor_core::crd::{PortAllocation, PortRequest};

/// Ports in use by *other* servers, keyed by (port, protocol) -> owner.
pub type Used = HashMap<(u16, cubeharbor_core::crd::Protocol), String>;

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
    // Pass 1: pinned and sticky ports, so a new request earlier in the list can't take them.
    for (i, req) in requests.iter().enumerate() {
        let port = if let Some(pinned) = req.public_port {
            if let Some(other) = used.get(&(pinned, req.protocol)) {
                bail!("public port {pinned}/{} for `{}` is already used by {other}", req.protocol.as_str(), req.name);
            }
            pinned
        } else if let Some(a) = current.iter().find(|a| {
            a.name == req.name && a.protocol == req.protocol && !used.contains_key(&(a.public_port, req.protocol))
        }) {
            a.public_port
        } else {
            continue;
        };
        if !taken.insert((port, req.protocol)) {
            bail!("port {port}/{} requested twice by {owner}", req.protocol.as_str());
        }
        ports[i] = Some(port);
    }
    // Pass 2: everything else gets the lowest free port.
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

    #[test]
    fn exhaustion() {
        assert!(allocate("me", &[req("a", None), req("b", None)], &[], &Used::new(), (100, 100)).is_err());
    }
}
