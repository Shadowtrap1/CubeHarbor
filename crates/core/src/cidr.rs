//! Minimal CIDR matching for trusted-proxy lists (gateway PROXY protocol, dashboard X-Forwarded-For).

use std::net::IpAddr;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let (ip, prefix) = s.split_once('/').map_or((s, None), |(a, b)| (a, Some(b)));
        let net: IpAddr = ip.parse().with_context(|| format!("invalid address in `{s}`"))?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.parse::<u8>().with_context(|| format!("invalid prefix in `{s}`"))?,
            None => max,
        };
        if prefix > max {
            bail!("prefix too long in `{s}`");
        }
        Ok(Self { net, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => mask_eq(u32::from(n) as u128, u32::from(a) as u128, self.prefix, 32),
            (IpAddr::V6(n), IpAddr::V6(a)) => mask_eq(u128::from(n), u128::from(a), self.prefix, 128),
            _ => false,
        }
    }
}

fn mask_eq(a: u128, b: u128, prefix: u8, bits: u32) -> bool {
    if prefix == 0 {
        return true;
    }
    let shift = bits - u32::from(prefix);
    (a >> shift) == (b >> shift)
}

pub fn parse_list(s: &str) -> Result<Vec<Cidr>> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(Cidr::parse).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let l = parse_list("10.0.0.0/8, 192.168.1.5, fd00::/8").unwrap();
        let has = |ip: &str| l.iter().any(|c| c.contains(ip.parse().unwrap()));
        assert!(has("10.42.0.7"));
        assert!(has("192.168.1.5"));
        assert!(!has("192.168.1.6"));
        assert!(has("fd12::1"));
        assert!(has("::ffff:10.1.2.3"));
        assert!(!has("8.8.8.8"));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("1.2.3.4".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
    }
}
