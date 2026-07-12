//! Canonical address policy for server-side outbound HTTP connections.
//!
//! Callers must validate every address returned by DNS and then pin the
//! validated set in their HTTP client. Validating only the URL text leaves a
//! DNS-rebinding window between policy evaluation and connection setup.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundHttpPolicy {
    PublicHttps,
    NumericLoopback,
}

#[derive(Debug, thiserror::Error)]
pub enum OutboundHttpError {
    #[error("outbound URL is invalid")]
    InvalidUrl,
    #[error("outbound URL violates the endpoint policy")]
    EndpointPolicy,
    #[error("outbound host resolution failed")]
    ResolutionFailed,
    #[error("outbound host resolved to no addresses")]
    NoAddresses,
    #[error("outbound host resolved to a forbidden address")]
    ForbiddenAddress,
    #[error("outbound HTTP client configuration failed")]
    ClientConfiguration,
}

#[derive(Clone)]
pub struct PinnedHttpEndpoint {
    pub url: reqwest::Url,
    pub client: reqwest::Client,
    pub addresses: Vec<SocketAddr>,
}

/// Resolve, validate, and pin one exact HTTP endpoint. The returned client
/// ignores proxy environment variables and never follows redirects, so a
/// validated public endpoint cannot pivot to a private target after the check.
pub async fn resolve_pinned_http_endpoint(
    raw_url: &str,
    policy: OutboundHttpPolicy,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<PinnedHttpEndpoint, OutboundHttpError> {
    let url = validate_http_endpoint(raw_url, policy)?;
    let host = url.host_str().ok_or(OutboundHttpError::EndpointPolicy)?;
    let port = url.port_or_known_default().ok_or(OutboundHttpError::EndpointPolicy)?;
    let literal = parse_host_ip(host);
    let addresses = if let Some(address) = literal {
        vec![SocketAddr::new(address, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| OutboundHttpError::ResolutionFailed)?
            .collect::<Vec<_>>()
    };
    validate_resolved_addresses(policy, &addresses)?;

    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .timeout(request_timeout);
    if literal.is_none() {
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let client = builder.build().map_err(|_| OutboundHttpError::ClientConfiguration)?;
    Ok(PinnedHttpEndpoint { url, client, addresses })
}

pub fn validate_http_endpoint(raw_url: &str, policy: OutboundHttpPolicy) -> Result<reqwest::Url, OutboundHttpError> {
    let url = reqwest::Url::parse(raw_url.trim()).map_err(|_| OutboundHttpError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(OutboundHttpError::EndpointPolicy);
    }
    let literal = url.host_str().and_then(parse_host_ip);
    match policy {
        OutboundHttpPolicy::PublicHttps => {
            if url.scheme() != "https" || literal.is_some_and(|address| !is_public_outbound_address(address)) {
                return Err(OutboundHttpError::ForbiddenAddress);
            }
        }
        OutboundHttpPolicy::NumericLoopback => {
            if !literal.is_some_and(is_numeric_loopback) {
                return Err(OutboundHttpError::ForbiddenAddress);
            }
        }
    }
    Ok(url)
}

pub fn validate_resolved_addresses(
    policy: OutboundHttpPolicy,
    addresses: &[SocketAddr],
) -> Result<(), OutboundHttpError> {
    if addresses.is_empty() {
        return Err(OutboundHttpError::NoAddresses);
    }
    let allowed = addresses.iter().all(|address| match policy {
        OutboundHttpPolicy::PublicHttps => is_public_outbound_address(address.ip()),
        OutboundHttpPolicy::NumericLoopback => is_numeric_loopback(address.ip()),
    });
    if !allowed {
        return Err(OutboundHttpError::ForbiddenAddress);
    }
    Ok(())
}

fn parse_host_ip(host: &str) -> Option<IpAddr> {
    host.trim_matches(|character| matches!(character, '[' | ']'))
        .parse()
        .ok()
}

/// Whether an address may be contacted by an Internet-facing provider.
///
/// This deliberately uses an allowlist-shaped definition. IPv4 special-use
/// ranges and IPv6 addresses outside global unicast are rejected, including
/// loopback, RFC1918/ULA, link-local, metadata, shared, benchmark,
/// documentation, multicast, reserved, and unspecified addresses.
pub fn is_public_outbound_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

/// Local providers are intentionally narrower than general private network
/// access: only a numeric loopback literal is accepted.
pub fn is_numeric_loopback(address: IpAddr) -> bool {
    address.is_loopback()
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    if a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
    {
        return false;
    }

    // IANA special-purpose and documentation networks. 169.254/16 includes
    // the cloud metadata endpoint 169.254.169.254 and was rejected above.
    !matches!(
        (a, b, c),
        (192, 0, 0) | (192, 0, 2) | (192, 88, 99) | (198, 51, 100) | (203, 0, 113)
    )
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    let global_unicast = (segments[0] & 0xe000) == 0x2000;
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    global_unicast && !documentation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_policy_rejects_special_ipv4_ranges() {
        for value in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!is_public_outbound_address(value.parse().unwrap()), "{value}");
        }
        assert!(is_public_outbound_address("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn public_policy_rejects_special_and_mapped_ipv6_ranges() {
        for value in [
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!is_public_outbound_address(value.parse().unwrap()), "{value}");
        }
        assert!(is_public_outbound_address("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn endpoint_policy_is_fail_closed_for_literals_and_dual_stack_dns() {
        assert!(validate_http_endpoint("https://8.8.8.8/v1/models", OutboundHttpPolicy::PublicHttps).is_ok());
        assert!(validate_http_endpoint("http://8.8.8.8/v1/models", OutboundHttpPolicy::PublicHttps).is_err());
        assert!(validate_http_endpoint("http://127.0.0.1:11434/v1", OutboundHttpPolicy::NumericLoopback).is_ok());
        assert!(validate_http_endpoint("http://localhost:11434/v1", OutboundHttpPolicy::NumericLoopback).is_err());

        let public = [
            "8.8.8.8:443".parse().unwrap(),
            "[2606:4700:4700::1111]:443".parse().unwrap(),
        ];
        assert!(validate_resolved_addresses(OutboundHttpPolicy::PublicHttps, &public).is_ok());
        let poisoned = ["8.8.8.8:443".parse().unwrap(), "[::1]:443".parse().unwrap()];
        assert!(validate_resolved_addresses(OutboundHttpPolicy::PublicHttps, &poisoned).is_err());
    }
}
