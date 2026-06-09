//! mDNS responder wiring.
//!
//! The backend is chosen by Cargo feature (see `Cargo.toml`):
//! - `astro-dnssd` → macOS Bonjour,
//! - `avahi`       → Linux avahi over D-Bus (the deploy target),
//! - neither       → the built-in raw-socket responder (cross-platform, but
//!   conflicts with a system responder on UDP 5353).
//!
//! Vendored from the `rs-matter` examples (`examples/src/common/mdns.rs`).

use rs_matter::error::Error;
use rs_matter::{crypto::Crypto, Matter};

#[allow(unused)]
pub async fn run_mdns<C: Crypto>(matter: &Matter<'_>, crypto: C) -> Result<(), Error> {
    #[cfg(feature = "astro-dnssd")]
    rs_matter::transport::network::mdns::astro::AstroMdnsResponder::new()
        .run(matter)
        .await?;

    #[cfg(all(feature = "avahi", not(feature = "astro-dnssd")))]
    rs_matter::transport::network::mdns::avahi::AvahiMdnsResponder::new(
        rs_matter::utils::zbus::Connection::system()
            .await
            .map_err(|e| {
                log::error!("avahi: cannot connect to system D-Bus: {e}");
                rs_matter::error::ErrorCode::StdIoError
            })?,
    )
    .run(matter)
    .await?;

    #[cfg(not(any(feature = "astro-dnssd", feature = "avahi")))]
    run_builtin_mdns(matter, crypto).await?;

    Ok(())
}

#[cfg(not(any(feature = "astro-dnssd", feature = "avahi")))]
async fn run_builtin_mdns<C: Crypto>(matter: &Matter<'_>, crypto: C) -> Result<(), Error> {
    use std::net::UdpSocket;

    use socket2::{Domain, Protocol, Socket, Type};

    use rs_matter::transport::network::mdns::builtin::{BuiltinMdnsResponder, Host};
    use rs_matter::transport::network::mdns::{
        MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
    };

    let (ipv4_addr, ipv6_addr, interface) = initialize_network()?;

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_only_v6(false)?;
    socket.bind(&MDNS_SOCKET_DEFAULT_BIND_ADDR.into())?;
    let socket = async_io::Async::<UdpSocket>::new_nonblocking(socket.into())?;

    socket
        .get_ref()
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, interface)?;
    socket
        .get_ref()
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &ipv4_addr)?;

    BuiltinMdnsResponder::new()
        .run(
            &socket,
            &socket,
            &Host {
                // Placeholder mDNS host label. Give each instance a unique value
                // if running more than one built-in responder on a network.
                hostname: "001122334455",
                ip: ipv4_addr,
                ipv6: ipv6_addr,
            },
            Some(ipv4_addr),
            Some(interface),
            matter,
            crypto,
        )
        .await
}

/// Pick a LAN interface that has both an IPv6 and a non-loopback IPv4 address.
#[cfg(not(any(feature = "astro-dnssd", feature = "avahi")))]
#[inline(never)]
fn initialize_network() -> Result<
    (
        rs_matter::transport::network::Ipv4Addr,
        rs_matter::transport::network::Ipv6Addr,
        u32,
    ),
    Error,
> {
    use log::{debug, error, info, warn};
    use rs_matter::error::ErrorCode;

    let all = if_addrs::get_if_addrs().map_err(|_| ErrorCode::StdIoError)?;
    debug!("Available network interfaces: {all:?}");

    let find_ipv6_candidate = |ipv6_filter: fn(std::net::Ipv6Addr) -> bool| {
        all.iter()
            .filter(|ia| !ia.is_loopback())
            .filter_map(|ia| match ia.addr {
                if_addrs::IfAddr::V6(ref v6) if ipv6_filter(v6.ip) => {
                    Some((ia.name.clone(), v6.ip, ia.index.unwrap_or(0)))
                }
                _ => None,
            })
            .find_map(|(iname, ipv6, index)| {
                all.iter()
                    .filter(|ia2| ia2.name == iname)
                    .find_map(|ia2| match ia2.addr {
                        if_addrs::IfAddr::V4(ref v4) => Some((iname.clone(), v4.ip, ipv6, index)),
                        _ => None,
                    })
            })
    };

    // Last-resort fallback for IPv4-only hosts (e.g. a container with no IPv6
    // assigned): accept an `eth*`/`eno*` interface with a non-loopback IPv4.
    let find_fallback_candidate = || {
        all.iter()
            .filter(|ia| !ia.is_loopback())
            .filter(|ia| ia.name.starts_with("eth") || ia.name.starts_with("eno"))
            .find_map(|ia| match ia.addr {
                if_addrs::IfAddr::V4(ref v4) => Some((
                    ia.name.clone(),
                    v4.ip,
                    std::net::Ipv6Addr::UNSPECIFIED,
                    ia.index.unwrap_or(0),
                )),
                _ => None,
            })
    };

    let candidate = find_ipv6_candidate(|ip| ip.is_unicast_link_local())
        .or_else(|| find_ipv6_candidate(|_| true))
        .or_else(|| {
            warn!("No interface with a usable IPv6 address; falling back to IPv4-only");
            find_fallback_candidate()
        })
        .ok_or_else(|| {
            error!("Cannot find network interface suitable for mDNS broadcasting");
            ErrorCode::StdIoError
        })?;

    let (iname, ip, ipv6, index) = candidate;
    if ipv6 == std::net::Ipv6Addr::UNSPECIFIED {
        warn!("Using interface {iname} without a link-local IPv6 address");
    }
    info!("Will use network interface {iname} with {ip}/{ipv6} for mDNS");

    Ok((ip.octets().into(), ipv6.octets().into(), index))
}
