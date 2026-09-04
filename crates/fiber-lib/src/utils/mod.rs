pub mod actor;
pub(crate) mod arithmetic;
pub mod encrypt_decrypt_file;
pub(crate) mod payment;
pub mod tx;

use tentacle::multiaddr::{Multiaddr, Protocol};
use tentacle::utils::{is_reachable, multiaddr_to_socketaddr, multiaddr_to_udp_socketaddr};

/// Check whether a multiaddr is publicly reachable.
///
/// For IP-based addresses (`Ip4`/`Ip6`), this extracts a TCP socket address, or
/// a UDP socket address for QUIC v1 only, and delegates to tentacle's
/// `is_reachable` check. Other UDP addresses are not dialable and are rejected.
///
/// For DNS-based addresses (`Dns4`/`Dns6`), we treat them as reachable because a DNS name implies
/// a publicly resolvable endpoint, except for DNS QUIC addresses, which Tentacle does not support.
///
/// For Tor onion addresses (`Onion3`), we treat them as always reachable
/// because they are publicly accessible via the Tor network.
pub(crate) fn is_addr_reachable(addr: &Multiaddr) -> bool {
    let transport_type = tentacle::utils::find_type(addr);
    let has_udp_protocol = addr.iter().any(|proto| matches!(proto, Protocol::Udp(_)));

    // Tentacle supports UDP addresses only as part of its QUIC v1 transport. A bare UDP
    // multiaddr is not dialable and must not be retained or advertised as reachable.
    if has_udp_protocol && transport_type != tentacle::utils::TransportType::QuicV1 {
        return false;
    }

    let has_dns_protocol = addr
        .iter()
        .any(|proto| matches!(proto, Protocol::Dns4(_) | Protocol::Dns6(_)));

    // Tentacle QUIC currently accepts IP addresses only. Do not retain or gossip a DNS QUIC
    // address that peers cannot dial.
    if has_dns_protocol && transport_type == tentacle::utils::TransportType::QuicV1 {
        return false;
    }

    let has_public_protocol = addr.iter().any(|proto| {
        matches!(
            proto,
            Protocol::Dns4(_) | Protocol::Dns6(_) | Protocol::Onion3(_)
        )
    });

    if has_public_protocol {
        return true;
    }

    multiaddr_to_socketaddr(addr)
        .or_else(|| {
            (transport_type == tentacle::utils::TransportType::QuicV1)
                .then(|| multiaddr_to_udp_socketaddr(addr))
                .flatten()
        })
        .map(|socket_addr| is_reachable(socket_addr.ip()))
        .unwrap_or_default()
}
