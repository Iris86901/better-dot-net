//! Destination policy: blocks private, loopback, link-local (cloud metadata), and non-web ports.
use crate::dns::Resolver;
use anyhow::{Result, anyhow, bail};
use ipnet::{Ipv4Net, Ipv6Net};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use wisp_mux::packet::ConnectPacket;

const BLOCKED_V4: [Ipv4Net; 15] = [
    Ipv4Net::new_assert(Ipv4Addr::new(0, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(10, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(100, 64, 0, 0), 10),
    Ipv4Net::new_assert(Ipv4Addr::new(127, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16),
    Ipv4Net::new_assert(Ipv4Addr::new(172, 16, 0, 0), 12),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 0, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 2, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 88, 99, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 168, 0, 0), 16),
    Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 15),
    Ipv4Net::new_assert(Ipv4Addr::new(198, 51, 100, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(203, 0, 113, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(224, 0, 0, 0), 4),
    Ipv4Net::new_assert(Ipv4Addr::new(240, 0, 0, 0), 4),
];

const BLOCKED_V6: [Ipv6Net; 12] = [
    Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
    Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
    Ipv6Net::new_assert(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    Ipv6Net::new_assert(Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    Ipv6Net::new_assert(Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    Ipv6Net::new_assert(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    Ipv6Net::new_assert(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    Ipv6Net::new_assert(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    Ipv6Net::new_assert(Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

pub async fn resolve(packet: &ConnectPacket, resolver: &Resolver) -> Result<IpAddr> {
    let host = check_host_and_port(packet)?;
    let addresses = resolver.lookup(&host).await?;
    pick_address(&addresses).ok_or_else(|| anyhow!("address blocked"))
}

pub fn is_public_host_or_ip(raw: &str) -> bool {
    let host = raw
        .trim()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !host_allowed(&host) {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return public_ip(ip);
    }
    true
}

pub fn is_metadata_ip(raw: &str) -> bool {
    let host = raw.trim().trim_matches(['[', ']']);
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    let link_v4 = Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16);
    let link_v6 = Ipv6Net::new_assert(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10);
    match ip {
        IpAddr::V4(v4) => link_v4.contains(&v4),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| link_v6.contains(&v6), |v4| link_v4.contains(&v4)),
    }
}

fn check_host_and_port(packet: &ConnectPacket) -> Result<String> {
    if !port_allowed(packet.port) {
        bail!("port blocked");
    }
    let host = packet.host.trim_end_matches('.').to_ascii_lowercase();
    if !host_allowed(&host) {
        bail!("host blocked");
    }
    Ok(host)
}

fn port_allowed(port: u16) -> bool {
    matches!(port, 80 | 443 | 8080 | 8443)
}

fn host_allowed(host: &str) -> bool {
    let blocked = host.is_empty()
        || host.len() > 253
        || host
            .bytes()
            .any(|c| !c.is_ascii_alphanumeric() && !b".-:".contains(&c))
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal");
    !blocked
}

fn pick_address(addresses: &[IpAddr]) -> Option<IpAddr> {
    if addresses.is_empty() || addresses.iter().any(|x| !public_ip(*x)) {
        return None;
    }
    addresses
        .iter()
        .find(|x| x.is_ipv4())
        .or(addresses.first())
        .copied()
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !BLOCKED_V4.iter().any(|net| net.contains(&v4)),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return !BLOCKED_V4.iter().any(|net| net.contains(&v4));
            }
            !BLOCKED_V6.iter().any(|net| net.contains(&v6))
        }
    }
}
