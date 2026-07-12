use std::net::IpAddr;
use std::time::Duration;

use aionui_common::outbound::{
    OutboundHttpPolicy, PinnedHttpEndpoint, is_numeric_loopback, resolve_pinned_http_endpoint,
};

use crate::error::SystemError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn resolve_provider_endpoint(
    platform: &str,
    url: &str,
    request_timeout: Duration,
) -> Result<PinnedHttpEndpoint, SystemError> {
    resolve(url, provider_policy(platform), request_timeout).await
}

pub(crate) async fn resolve_probe_endpoint(
    url: &str,
    request_timeout: Duration,
) -> Result<PinnedHttpEndpoint, SystemError> {
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|_| SystemError::BadRequest("baseUrl must be a valid HTTP URL".into()))?;
    let literal = parsed.host_str().and_then(parse_host_ip);
    let policy = if literal.is_some_and(is_numeric_loopback) {
        OutboundHttpPolicy::NumericLoopback
    } else {
        OutboundHttpPolicy::PublicHttps
    };
    resolve(url, policy, request_timeout).await
}

async fn resolve(
    url: &str,
    policy: OutboundHttpPolicy,
    request_timeout: Duration,
) -> Result<PinnedHttpEndpoint, SystemError> {
    resolve_pinned_http_endpoint(url, policy, CONNECT_TIMEOUT.min(request_timeout), request_timeout)
        .await
        .map_err(|_| SystemError::BadRequest("provider endpoint failed outbound policy validation".into()))
}

fn provider_policy(platform: &str) -> OutboundHttpPolicy {
    if matches!(
        platform.trim().to_ascii_lowercase().as_str(),
        "ollama" | "local" | "llama.cpp" | "llamacpp"
    ) {
        OutboundHttpPolicy::NumericLoopback
    } else {
        OutboundHttpPolicy::PublicHttps
    }
}

fn parse_host_ip(host: &str) -> Option<IpAddr> {
    host.trim_matches(|character| matches!(character, '[' | ']'))
        .parse()
        .ok()
}
