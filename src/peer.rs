//! Peer-to-peer iroh transport behind a share code.
//!
//! The share code carries everything a viewer needs to reach a source: the endpoint id,
//! its relay URL and its direct addresses. Nothing is published to a discovery service,
//! so holding the code is the only way to find the source — and also the only thing
//! needed to watch it. **Treat a share code as a secret.** Restricting a source to
//! specific viewer identities is not implemented.
//!
//! Relays carry connection setup and holepunch coordination. The media path itself is
//! required to become direct: see [`require_direct`]. A relayed path is refused rather
//! than used, so a connection that cannot be holepunched fails instead of sending the
//! stream through a third party's server.

use anyhow::{Context, Result, bail, ensure};
use data_encoding::BASE32_NOPAD;
use moq_tokio::iroh::{
    Endpoint,
    web_transport_iroh::iroh::{
        EndpointAddr, EndpointId, RelayUrl, TransportAddr, Watcher,
        address_lookup::MemoryLookup,
        endpoint::{TransportAddrUsage, default_relay_mode, presets},
    },
};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

const VERSION: u8 = 1;
const KIND_IPV4: u8 = 1;
const KIND_IPV6: u8 = 2;
const KIND_RELAY: u8 = 3;
/// A code has to stay pasteable, and a source with this many addresses is misconfigured.
const MAX_ADDRESSES: usize = 32;
const MAX_RELAY_URL: usize = 512;

/// Bind an endpoint that can reach peers across the internet without publishing
/// anything about itself.
///
/// `peer` seeds a known source's addresses, which replaces the discovery service a
/// viewer would otherwise need. A source passes `None`: viewers come to it.
pub async fn endpoint(peer: Option<EndpointAddr>) -> Result<Endpoint> {
    // `Minimal` sets only the crypto provider, so no address is published or resolved.
    // The relay mode is the one thing added back, for holepunch coordination.
    let mut builder = Endpoint::builder(presets::Minimal).relay_mode(default_relay_mode());
    if let Some(addr) = peer {
        let lookup = MemoryLookup::new();
        lookup.add_endpoint_info(addr);
        builder = builder.address_lookup(lookup);
    }
    Ok(builder.bind().await?)
}

/// The code a viewer needs, once a relay and at least one direct address are known.
pub async fn share_code(endpoint: &Endpoint, within: Duration) -> Result<String> {
    let deadline = tokio::time::Instant::now() + within;
    tokio::time::timeout_at(deadline, endpoint.online())
        .await
        .context("no relay connection; a share code cannot be holepunched to")?;
    let mut watcher = endpoint.watch_addr();
    loop {
        let addr = watcher.get();
        let relays = addr
            .addrs
            .iter()
            .any(|entry| matches!(entry, TransportAddr::Relay(_)));
        let direct = addr
            .addrs
            .iter()
            .any(|entry| matches!(entry, TransportAddr::Ip(_)));
        if relays && direct {
            return encode(&addr);
        }
        tokio::time::timeout_at(deadline, watcher.updated())
            .await
            .context("no relay and direct address to put in a share code")?
            .context("the endpoint stopped reporting addresses")?;
    }
}

/// Encode an endpoint address as one pasteable token.
pub fn encode(addr: &EndpointAddr) -> Result<String> {
    let mut bytes = vec![VERSION];
    bytes.extend_from_slice(addr.id.as_bytes());
    let mut count = 0;
    for entry in &addr.addrs {
        match entry {
            TransportAddr::Ip(SocketAddr::V4(socket)) => {
                bytes.push(KIND_IPV4);
                bytes.extend_from_slice(&socket.ip().octets());
                bytes.extend_from_slice(&socket.port().to_be_bytes());
            }
            TransportAddr::Ip(SocketAddr::V6(socket)) => {
                bytes.push(KIND_IPV6);
                bytes.extend_from_slice(&socket.ip().octets());
                bytes.extend_from_slice(&socket.port().to_be_bytes());
            }
            TransportAddr::Relay(url) => {
                let text = url.to_string();
                ensure!(text.len() <= MAX_RELAY_URL, "relay URL is too long to share");
                bytes.push(KIND_RELAY);
                bytes.extend_from_slice(&(text.len() as u16).to_be_bytes());
                bytes.extend_from_slice(text.as_bytes());
            }
            // A custom transport cannot be reconstructed from a code, and silently
            // dropping it would produce a code that reaches less than it appears to.
            other => bail!("cannot share the address {other}"),
        }
        count += 1;
        ensure!(count <= MAX_ADDRESSES, "too many addresses to share");
    }
    // An id alone cannot be connected to without a discovery service, which this
    // transport deliberately does not use.
    ensure!(count > 0, "the endpoint has no address to share");
    Ok(BASE32_NOPAD.encode(&bytes).to_lowercase())
}

/// Decode a share code. Rejects anything it cannot reconstruct exactly.
pub fn decode(code: &str) -> Result<EndpointAddr> {
    let bytes = BASE32_NOPAD
        .decode(code.trim().to_uppercase().as_bytes())
        .context("a share code is base32 text")?;
    ensure!(
        bytes.len() >= 33 && bytes[0] == VERSION,
        "unsupported share code"
    );
    let id = EndpointId::from_bytes(bytes[1..33].try_into().unwrap())
        .context("invalid endpoint id in the share code")?;
    let mut addr = EndpointAddr::new(id);
    let mut rest = &bytes[33..];
    let mut count = 0;
    while let Some((kind, body)) = rest.split_first() {
        let used = match *kind {
            KIND_IPV4 => {
                ensure!(body.len() >= 6, "truncated IPv4 address");
                let ip = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
                let port = u16::from_be_bytes([body[4], body[5]]);
                addr = addr.with_ip_addr(SocketAddr::new(IpAddr::V4(ip), port));
                6
            }
            KIND_IPV6 => {
                ensure!(body.len() >= 18, "truncated IPv6 address");
                let octets: [u8; 16] = body[..16].try_into().unwrap();
                let port = u16::from_be_bytes([body[16], body[17]]);
                addr = addr.with_ip_addr(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port));
                18
            }
            KIND_RELAY => {
                ensure!(body.len() >= 2, "truncated relay URL");
                let length = u16::from_be_bytes([body[0], body[1]]) as usize;
                ensure!(body.len() >= 2 + length, "truncated relay URL");
                let text = std::str::from_utf8(&body[2..2 + length]).context("relay URL")?;
                addr = addr.with_relay_url(text.parse::<RelayUrl>().context("relay URL")?);
                2 + length
            }
            other => bail!("unsupported address kind {other} in the share code"),
        };
        rest = &body[used..];
        count += 1;
        ensure!(count <= MAX_ADDRESSES, "too many addresses in the share code");
    }
    ensure!(count > 0, "the share code carries no address to connect to");
    Ok(addr)
}

/// The direct addresses from an address, for a dial that takes socket addresses.
pub fn direct_addresses(addr: &EndpointAddr) -> Vec<SocketAddr> {
    addr.addrs
        .iter()
        .filter_map(|entry| match entry {
            TransportAddr::Ip(socket) => Some(*socket),
            _ => None,
        })
        .collect()
}

/// Fail unless the path to `peer` became a direct one.
///
/// iroh starts a connection over the relay and upgrades it once holepunching succeeds,
/// so the first frames of a stream can still traverse a relay before this returns.
pub async fn require_direct(endpoint: &Endpoint, peer: EndpointId, within: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if let Some(info) = endpoint.remote_info(peer).await {
            if info.addrs().any(|entry| {
                matches!(entry.usage(), TransportAddrUsage::Active)
                    && matches!(entry.addr(), TransportAddr::Ip(_))
            }) {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "no direct peer-to-peer path to {peer} within {within:?}. \
                 Relayed media is refused, so the stream stops here."
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> EndpointAddr {
        let id = moq_tokio::iroh::web_transport_iroh::iroh::SecretKey::generate().public();
        EndpointAddr::new(id)
            .with_ip_addr("192.0.2.7:41641".parse().unwrap())
            .with_ip_addr("[2001:db8::1]:41641".parse().unwrap())
            .with_relay_url("https://relay.example./".parse().unwrap())
    }

    #[test]
    fn a_share_code_round_trips_every_address_it_carries() {
        let addr = address();
        let code = encode(&addr).unwrap();
        assert_eq!(code, code.to_lowercase());
        let decoded = decode(&code).unwrap();
        assert_eq!(decoded, addr);
        // Pasted codes pick up whitespace and case changes on the way.
        assert_eq!(decode(&format!("  {}\n", code.to_uppercase())).unwrap(), addr);
        assert_eq!(direct_addresses(&decoded).len(), 2);
    }

    #[test]
    fn a_peer_with_no_direct_path_is_refused_rather_than_relayed() {
        // A loopback-only endpoint reaches no relay and no peer, which is what a failed
        // holepunch looks like to `require_direct`.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let endpoint = Endpoint::builder(presets::Minimal)
                .clear_relay_transports()
                .clear_address_lookup()
                .bind_addr("127.0.0.1:0".parse::<SocketAddr>().unwrap())
                .unwrap()
                .bind()
                .await
                .unwrap();
            let absent = moq_tokio::iroh::web_transport_iroh::iroh::SecretKey::generate().public();
            let error = require_direct(&endpoint, absent, Duration::from_millis(300))
                .await
                .expect_err("a peer with no path must be refused");
            assert!(error.to_string().contains("no direct peer-to-peer path"));
            endpoint.close().await;
        });
    }

    #[test]
    fn a_damaged_share_code_is_refused_rather_than_partly_read() {
        let code = encode(&address()).unwrap();
        assert!(decode("").is_err());
        assert!(decode("not base32 at all!").is_err());
        // Truncation inside the trailing address list must not yield a usable address.
        let raw = BASE32_NOPAD.decode(code.to_uppercase().as_bytes()).unwrap();
        for cut in [1, 20, 33, raw.len() - 3] {
            assert!(decode(&BASE32_NOPAD.encode(&raw[..cut]).to_lowercase()).is_err());
        }
        let mut wrong_version = raw.clone();
        wrong_version[0] = VERSION + 1;
        assert!(decode(&BASE32_NOPAD.encode(&wrong_version).to_lowercase()).is_err());
    }
}
