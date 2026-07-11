use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use futures_util::TryStreamExt;
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderMap, HeaderName,
    HeaderValue, IF_RANGE, LAST_MODIFIED, RANGE,
};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::supervisor::{configured_worker_socket, managed_worker_requested};
use crate::{KnowledgeConfigError, KnowledgeError};

const INTERNAL_TOKEN_HEADER: &str = "x-centaurai-internal-token";
const MAX_JSON_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub(crate) struct WorkerClient {
    client: reqwest::Client,
    stream_client: reqwest::Client,
    base_url: Url,
    token: String,
    transport_label: &'static str,
}

pub(crate) struct WorkerContentResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Body,
}

impl fmt::Debug for WorkerClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerClient")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .field("transport_label", &self.transport_label)
            .finish()
    }
}

impl WorkerClient {
    pub(crate) fn from_environment(data_dir: &Path) -> Result<Option<Self>, KnowledgeConfigError> {
        let token = std::env::var("CENTAURAI_KNOWLEDGE_INTERNAL_TOKEN")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let Some(token) = token else {
            return Ok(None);
        };

        if std::env::var_os("CENTAURAI_KNOWLEDGE_WORKER_SOCKET").is_some() || managed_worker_requested() {
            return Self::for_unix_socket(configured_worker_socket(data_dir)?, token).map(Some);
        }
        if let Ok(endpoint) = std::env::var("CENTAURAI_KNOWLEDGE_WORKER_URL") {
            return Self::for_loopback_url(&endpoint, token).map(Some);
        }

        #[cfg(unix)]
        {
            Self::for_unix_socket(configured_worker_socket(data_dir)?, token).map(Some)
        }
        #[cfg(not(unix))]
        {
            let _ = data_dir;
            Ok(None)
        }
    }

    pub(crate) fn for_loopback_url(endpoint: &str, token: String) -> Result<Self, KnowledgeConfigError> {
        let base_url = validate_loopback_endpoint(endpoint)?;
        let client = client_builder(true)
            .build()
            .map_err(|_| KnowledgeConfigError::TransportInitialization)?;
        let stream_client = client_builder(false)
            .build()
            .map_err(|_| KnowledgeConfigError::TransportInitialization)?;
        Ok(Self {
            client,
            stream_client,
            base_url,
            token,
            transport_label: "loopback",
        })
    }

    #[cfg(unix)]
    pub(crate) fn for_unix_socket(socket: PathBuf, token: String) -> Result<Self, KnowledgeConfigError> {
        if !socket.is_absolute() {
            return Err(KnowledgeConfigError::InvalidSocketPath);
        }
        let client = client_builder(true)
            .unix_socket(socket.clone())
            .build()
            .map_err(|_| KnowledgeConfigError::TransportInitialization)?;
        let stream_client = client_builder(false)
            .unix_socket(socket)
            .build()
            .map_err(|_| KnowledgeConfigError::TransportInitialization)?;
        Ok(Self {
            client,
            stream_client,
            base_url: Url::parse("http://knowledge-worker/")
                .map_err(|_| KnowledgeConfigError::TransportInitialization)?,
            token,
            transport_label: "unix",
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn for_unix_socket(_socket: PathBuf, _token: String) -> Result<Self, KnowledgeConfigError> {
        Err(KnowledgeConfigError::InvalidSocketPath)
    }

    pub(crate) fn transport_label(&self) -> &'static str {
        self.transport_label
    }

    pub(crate) async fn get<T>(&self, path: &str, query: Option<&str>) -> Result<T, KnowledgeError>
    where
        T: DeserializeOwned,
    {
        self.request_json::<(), T>(Method::GET, path, query, None).await
    }

    pub(crate) async fn send<TRequest, TResponse>(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
        body: &TRequest,
    ) -> Result<TResponse, KnowledgeError>
    where
        TRequest: Serialize + ?Sized,
        TResponse: DeserializeOwned,
    {
        self.request_json(method, path, query, Some(body)).await
    }

    pub(crate) async fn delete<T>(&self, path: &str) -> Result<T, KnowledgeError>
    where
        T: DeserializeOwned,
    {
        self.request_json::<(), T>(Method::DELETE, path, None, None).await
    }

    pub(crate) async fn upload<T>(
        &self,
        content_type: HeaderValue,
        content_length: Option<HeaderValue>,
        body: Body,
    ) -> Result<T, KnowledgeError>
    where
        T: DeserializeOwned,
    {
        let url = self.url_for("/api/knowledge/sources", None)?;
        let stream = body.into_data_stream().map_err(std::io::Error::other);
        let mut request = self
            .client
            .post(url)
            .header(INTERNAL_TOKEN_HEADER, &self.token)
            .header(CONTENT_TYPE, content_type)
            .body(reqwest::Body::wrap_stream(stream));
        if let Some(content_length) = content_length {
            request = request.header(CONTENT_LENGTH, content_length);
        }
        let response = request.send().await.map_err(map_transport_error)?;
        decode_response(response).await
    }

    pub(crate) async fn source_content(
        &self,
        source_id: &str,
        download: Option<bool>,
        method: Method,
        range: Option<HeaderValue>,
        if_range: Option<HeaderValue>,
    ) -> Result<WorkerContentResponse, KnowledgeError> {
        let return_body = method == Method::GET;
        let path = format!("/api/knowledge/sources/{source_id}/content");
        let query = download.map(|download| format!("download={download}"));
        let url = self.url_for(&path, query.as_deref())?;
        let mut request = self
            .stream_client
            .request(method, url)
            .header(INTERNAL_TOKEN_HEADER, &self.token);
        if let Some(range) = range {
            request = request.header(RANGE, range);
        }
        if let Some(if_range) = if_range {
            request = request.header(IF_RANGE, if_range);
        }

        let response = request.send().await.map_err(map_transport_error)?;
        let status = response.status();
        match status {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                let headers = filtered_content_headers(response.headers());
                let body = if return_body {
                    let stream = response
                        .bytes_stream()
                        .map_err(|_| std::io::Error::other("knowledge source stream failed"));
                    Body::from_stream(stream)
                } else {
                    Body::empty()
                };
                Ok(WorkerContentResponse { status, headers, body })
            }
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(WorkerContentResponse {
                status,
                headers: filtered_content_headers(response.headers()),
                body: Body::empty(),
            }),
            StatusCode::NOT_FOUND => Err(KnowledgeError::NotFound),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => Err(KnowledgeError::InvalidRequest),
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => Err(KnowledgeError::Timeout),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(KnowledgeError::InvalidResponse),
            status if status.is_server_error() => Err(KnowledgeError::Unavailable),
            _ => Err(KnowledgeError::InvalidResponse),
        }
    }

    async fn request_json<TRequest, TResponse>(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
        body: Option<&TRequest>,
    ) -> Result<TResponse, KnowledgeError>
    where
        TRequest: Serialize + ?Sized,
        TResponse: DeserializeOwned,
    {
        let url = self.url_for(path, query)?;
        let mut request = self
            .client
            .request(method, url)
            .header(INTERNAL_TOKEN_HEADER, &self.token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(map_transport_error)?;
        decode_response(response).await
    }

    fn url_for(&self, path: &str, query: Option<&str>) -> Result<Url, KnowledgeError> {
        if !path.starts_with('/')
            || path.chars().any(|character| matches!(character, '\\' | '?' | '#'))
            || path.split('/').any(|segment| segment == "..")
        {
            return Err(KnowledgeError::InvalidRequest);
        }
        let mut url = self
            .base_url
            .join(path.trim_start_matches('/'))
            .map_err(|_| KnowledgeError::InvalidRequest)?;
        if let Some(query) = query {
            if query.len() > 8_192 || query.chars().any(|character| matches!(character, '\r' | '\n')) {
                return Err(KnowledgeError::InvalidRequest);
            }
            url.set_query(Some(query));
        }
        Ok(url)
    }
}

fn client_builder(with_request_timeout: bool) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT);
    if with_request_timeout {
        builder.timeout(REQUEST_TIMEOUT)
    } else {
        builder
    }
}

const CONTENT_RESPONSE_HEADERS: &[HeaderName] = &[
    CONTENT_TYPE,
    CONTENT_LENGTH,
    CONTENT_RANGE,
    CONTENT_DISPOSITION,
    ACCEPT_RANGES,
    ETAG,
    LAST_MODIFIED,
];

fn filtered_content_headers(headers: &HeaderMap) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for name in CONTENT_RESPONSE_HEADERS {
        if let Some(value) = headers.get(name) {
            filtered.insert(name.clone(), value.clone());
        }
    }
    filtered
}

pub(crate) fn validate_loopback_endpoint(endpoint: &str) -> Result<Url, KnowledgeConfigError> {
    let url = Url::parse(endpoint).map_err(|_| KnowledgeConfigError::InvalidEndpoint)?;
    if url.scheme() != "http" {
        return Err(KnowledgeConfigError::NonLoopbackEndpoint);
    }
    let is_loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(|character| matches!(character, '[' | ']'))
                .parse::<IpAddr>()
                .ok()
        })
        .is_some_and(|address| address.is_loopback());
    if !is_loopback {
        return Err(KnowledgeConfigError::NonLoopbackEndpoint);
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(KnowledgeConfigError::InvalidEndpointShape);
    }
    Ok(url)
}

fn map_transport_error(error: reqwest::Error) -> KnowledgeError {
    if error.is_timeout() {
        KnowledgeError::Timeout
    } else {
        KnowledgeError::Unavailable
    }
}

async fn decode_response<T>(response: reqwest::Response) -> Result<T, KnowledgeError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        return Err(map_upstream_status(status));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_JSON_RESPONSE_BYTES)
    {
        return Err(KnowledgeError::InvalidResponse);
    }
    let bytes = response.bytes().await.map_err(map_transport_error)?;
    if bytes.len() as u64 > MAX_JSON_RESPONSE_BYTES {
        return Err(KnowledgeError::InvalidResponse);
    }
    serde_json::from_slice(&bytes).map_err(|_| KnowledgeError::InvalidResponse)
}

fn map_upstream_status(status: StatusCode) -> KnowledgeError {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => KnowledgeError::InvalidRequest,
        StatusCode::NOT_FOUND => KnowledgeError::NotFound,
        StatusCode::CONFLICT => KnowledgeError::Conflict,
        StatusCode::PAYLOAD_TOO_LARGE => KnowledgeError::PayloadTooLarge,
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => KnowledgeError::Timeout,
        StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::TOO_MANY_REQUESTS
        | StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE => KnowledgeError::Unavailable,
        _ => KnowledgeError::InvalidResponse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_literal_loopback_http_endpoints() {
        assert!(validate_loopback_endpoint("http://127.0.0.1:8618/").is_ok());
        assert!(validate_loopback_endpoint("http://[::1]:8618/").is_ok());
        assert!(matches!(
            validate_loopback_endpoint("http://localhost:8618/"),
            Err(KnowledgeConfigError::NonLoopbackEndpoint)
        ));
        assert!(matches!(
            validate_loopback_endpoint("https://127.0.0.1:8618/"),
            Err(KnowledgeConfigError::NonLoopbackEndpoint)
        ));
        assert!(matches!(
            validate_loopback_endpoint("http://10.0.0.8:8618/"),
            Err(KnowledgeConfigError::NonLoopbackEndpoint)
        ));
    }

    #[test]
    fn rejects_endpoint_features_that_can_escape_the_fixed_origin() {
        for endpoint in [
            "http://user:secret@127.0.0.1:8618/",
            "http://127.0.0.1:8618/api",
            "http://127.0.0.1:8618/?next=http://example.com",
            "http://127.0.0.1:8618/#fragment",
        ] {
            assert!(matches!(
                validate_loopback_endpoint(endpoint),
                Err(KnowledgeConfigError::InvalidEndpointShape)
            ));
        }
    }

    #[test]
    fn upstream_status_mapping_never_exposes_worker_error_bodies() {
        assert!(matches!(
            map_upstream_status(StatusCode::BAD_REQUEST),
            KnowledgeError::InvalidRequest
        ));
        assert!(matches!(
            map_upstream_status(StatusCode::NOT_FOUND),
            KnowledgeError::NotFound
        ));
        assert!(matches!(
            map_upstream_status(StatusCode::CONFLICT),
            KnowledgeError::Conflict
        ));
        assert!(matches!(
            map_upstream_status(StatusCode::PAYLOAD_TOO_LARGE),
            KnowledgeError::PayloadTooLarge
        ));
        assert!(matches!(
            map_upstream_status(StatusCode::GATEWAY_TIMEOUT),
            KnowledgeError::Timeout
        ));
        assert!(matches!(
            map_upstream_status(StatusCode::UNAUTHORIZED),
            KnowledgeError::Unavailable
        ));
        assert!(matches!(
            map_upstream_status(StatusCode::PERMANENT_REDIRECT),
            KnowledgeError::InvalidResponse
        ));
    }
}
