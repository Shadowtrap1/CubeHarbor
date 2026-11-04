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
    let mut out = vec![];
    let mut taken: BTreeSet<(u16, cubeharbor_core::crd::Protocol)> = BTreeSet::new();
    for req in requests {
        let existing = current.iter().find(|a| {
            a.name == req.name && a.protocol == req.protocol && req.public_port.is_none_or(|p| p == a.public_port)
        });
        let port = if let Some(pinned) = req.public_port {
            if let Some(other) = used.get(&(pinned, req.protocol)) {
                bail!("public port {pinned}/{} for `{}` is already used by {other}", req.protocol.as_str(), req.name);
            }
            pinned
        } else if let Some(a) = existing.filter(|a| !used.contains_key(&(a.public_port, req.protocol))) {
            a.public_port
        } else {
            let free = (range.0..=range.1)
                .find(|p| !used.contains_key(&(*p, req.protocol)) && !taken.contains(&(*p, req.protocol)));
            match free {
                Some(p) => p,
                None => bail!("no free {} ports left in {}-{}", req.protocol.as_str(), range.0, range.1),
            }
        };
        if !taken.insert((port, req.protocol)) {
            bail!("port {port}/{} requested twice by {owner}", req.protocol.as_str());
        }
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

    #[test]
    fn exhaustion() {
        assert!(allocate("me", &[req("a", None), req("b", None)], &[], &Used::new(), (100, 100)).is_err());
    }
}
