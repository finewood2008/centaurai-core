use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use aionui_api_types::{BrainDefinition, BrainKind};
use aionui_common::decrypt_string;
use aionui_db::{IProviderRepository, models::Provider};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::ports::{
    BrainCatalogPort, BrainExecutionFailure, BrainExecutionFailureKind, BrainExecutionPort, BrainInvocation,
    BrainOpinion,
};

/// Executes provider-model brains without exposing decrypted credentials to a client.
///
/// OpenAI-compatible and Anthropic-compatible providers are supported directly.
/// ACP brains remain part of the public domain model and can be supplied by a
/// composite executor without weakening this provider credential boundary.
pub struct ProviderBrainRuntime {
    providers: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    client: reqwest::Client,
}

const MAX_PROVIDER_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const PROVIDER_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PROVIDER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(50);

impl ProviderBrainRuntime {
    pub fn new(providers: Arc<dyn IProviderRepository>, encryption_key: [u8; 32]) -> Self {
        Self {
            providers,
            encryption_key,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(PROVIDER_CONNECT_TIMEOUT)
                .timeout(PROVIDER_REQUEST_TIMEOUT)
                .build()
                .expect("static provider HTTP client configuration must be valid"),
        }
    }

    async fn provider(&self, id: &str) -> Result<Provider, BrainExecutionFailure> {
        self.providers
            .find_by_id(id)
            .await
            .map_err(|_| unavailable("provider catalog query failed"))?
            .filter(|provider| provider.enabled)
            .ok_or_else(|| unavailable("provider is missing or disabled"))
    }
}

#[async_trait::async_trait]
impl BrainCatalogPort for ProviderBrainRuntime {
    async fn available_brains(&self) -> Result<Vec<BrainDefinition>, BrainExecutionFailure> {
        let providers = self
            .providers
            .list()
            .await
            .map_err(|_| unavailable("provider catalog query failed"))?;
        let mut first_models = Vec::new();
        let mut remaining_models = Vec::new();

        for provider in providers.into_iter().filter(|provider| provider.enabled) {
            let models: Vec<String> = serde_json::from_str(&provider.models).unwrap_or_default();
            let enabled: HashMap<String, bool> = provider
                .model_enabled
                .as_deref()
                .and_then(|value| serde_json::from_str(value).ok())
                .unwrap_or_default();
            let mut models = models
                .into_iter()
                .filter(|model| enabled.get(model).copied().unwrap_or(true));
            if let Some(model) = models.next() {
                first_models.push(brain_for(&provider.id, model));
            }
            remaining_models.extend(models.map(|model| brain_for(&provider.id, model)));
        }
        first_models.extend(remaining_models);
        Ok(first_models)
    }
}

#[async_trait::async_trait]
impl BrainExecutionPort for ProviderBrainRuntime {
    async fn execute(&self, invocation: BrainInvocation) -> Result<BrainOpinion, BrainExecutionFailure> {
        if invocation.brain.kind != BrainKind::ProviderModel {
            return Err(BrainExecutionFailure::new(
                BrainExecutionFailureKind::Unsupported,
                "ACP brain requires an ACP execution adapter",
            ));
        }

        let provider = self.provider(&invocation.brain.provider_id).await?;
        let api_key = decrypt_string(&provider.api_key_encrypted, &self.encryption_key)
            .map_err(|_| unavailable("provider credential could not be decrypted"))?;
        let api_key = api_key
            .split([',', '\n'])
            .map(str::trim)
            .find(|value| !value.is_empty())
            .map(str::to_owned);
        if api_key.is_none() && !is_local_platform(&provider.platform) {
            return Err(unavailable("provider has no usable credential"));
        }
        let protocol = provider_protocol(&provider, &invocation.brain.model);
        let prompt = build_prompt(&invocation);
        let url = validated_provider_url(&provider, protocol == "anthropic")?;

        let request = if protocol == "anthropic" {
            let request = self
                .client
                .post(url)
                .header("anthropic-version", "2023-06-01")
                .json(&json!({
                    "model": invocation.brain.model,
                    "max_tokens": 2048,
                    "system": invocation.role.instructions,
                    "messages": [{"role": "user", "content": prompt}],
                }));
            if let Some(api_key) = api_key.as_deref() {
                request.header("x-api-key", api_key)
            } else {
                request
            }
        } else {
            let request = self.client.post(url).json(&json!({
                "model": invocation.brain.model,
                "messages": [
                    {"role": "system", "content": invocation.role.instructions},
                    {"role": "user", "content": prompt}
                ],
                "temperature": 0.2
            }));
            if let Some(api_key) = api_key.as_deref() {
                request.bearer_auth(api_key)
            } else {
                request
            }
        };

        let response = request.send().await.map_err(map_reqwest_error)?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(BrainExecutionFailure::new(
                BrainExecutionFailureKind::RateLimited,
                "provider rate limited the decision turn",
            ));
        }
        if status == reqwest::StatusCode::REQUEST_TIMEOUT || status == reqwest::StatusCode::GATEWAY_TIMEOUT {
            return Err(BrainExecutionFailure::new(
                BrainExecutionFailureKind::Timeout,
                "provider timed out",
            ));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(BrainExecutionFailure::new(
                BrainExecutionFailureKind::Unauthorized,
                "provider rejected its stored credential",
            ));
        }
        if !status.is_success() {
            return Err(unavailable(format!("provider returned HTTP {status}")));
        }
        let payload = limited_json(response).await?;
        let content = if protocol == "anthropic" {
            payload
                .get("content")
                .and_then(Value::as_array)
                .and_then(|items| items.iter().find_map(|item| item.get("text").and_then(Value::as_str)))
        } else {
            payload.pointer("/choices/0/message/content").and_then(Value::as_str)
        }
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| unavailable("provider response contained no opinion"))?;

        Ok(BrainOpinion {
            content: content.to_owned(),
            evidence: Vec::new(),
        })
    }
}

fn brain_for(provider_id: &str, model: String) -> BrainDefinition {
    BrainDefinition {
        id: None,
        kind: BrainKind::ProviderModel,
        provider_id: provider_id.to_owned(),
        model,
        role_id: String::new(),
        agent_id: None,
        tool_ids: Vec::new(),
    }
}

fn provider_protocol(provider: &Provider, model: &str) -> String {
    provider
        .model_protocols
        .as_deref()
        .and_then(|value| serde_json::from_str::<HashMap<String, String>>(value).ok())
        .and_then(|protocols| protocols.get(model).cloned())
        .unwrap_or_else(|| {
            if matches!(provider.platform.as_str(), "anthropic" | "claude") {
                "anthropic".to_owned()
            } else {
                "openai".to_owned()
            }
        })
}

fn provider_url(provider: &Provider, anthropic: bool) -> String {
    let base = provider.base_url.trim_end_matches('/');
    if provider.is_full_url {
        return base.to_owned();
    }
    if anthropic {
        if base.ends_with("/v1/messages") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    } else if base.ends_with("/chat/completions") {
        base.to_owned()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

fn validated_provider_url(provider: &Provider, anthropic: bool) -> Result<reqwest::Url, BrainExecutionFailure> {
    let url =
        reqwest::Url::parse(&provider_url(provider, anthropic)).map_err(|_| unavailable("provider URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(unavailable("provider URL violates the HTTP endpoint policy"));
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let address = host.parse::<IpAddr>().ok();
    if address.is_some_and(forbidden_ip) {
        return Err(unavailable("provider URL host is not allowed"));
    }
    let loopback = host == "localhost" || address.is_some_and(|address| address.is_loopback());
    if is_local_platform(&provider.platform) && !loopback {
        return Err(unavailable("local provider URL must use loopback"));
    }
    if !is_local_platform(&provider.platform) && loopback {
        return Err(unavailable("loopback provider URL requires an explicit local platform"));
    }
    Ok(url)
}

fn is_local_platform(platform: &str) -> bool {
    matches!(
        platform.trim().to_ascii_lowercase().as_str(),
        "ollama" | "local" | "llama.cpp" | "llamacpp"
    )
}

fn forbidden_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_unspecified()
                || address.is_link_local()
                || address.is_multicast()
                || address == Ipv4Addr::BROADCAST
        }
        IpAddr::V6(address) => address.is_unspecified() || address.is_unicast_link_local() || address.is_multicast(),
    }
}

async fn limited_json(response: reqwest::Response) -> Result<Value, BrainExecutionFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        return Err(unavailable("provider response exceeded the size limit"));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| unavailable("provider response stream failed"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(unavailable("provider response exceeded the size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| unavailable("provider returned an invalid response"))
}

fn build_prompt(invocation: &BrainInvocation) -> String {
    let mut prompt = format!(
        "Decision question:\n{}\n\nGive an independent recommendation, assumptions, risks, and concrete next actions.",
        invocation.question
    );
    if !invocation.interjections.is_empty() {
        prompt.push_str("\n\nOwner interjections:\n");
        prompt.push_str(&invocation.interjections.join("\n"));
    }
    if !invocation.evidence.is_empty() {
        prompt.push_str("\n\nRetrieved evidence (cite source_id when used):\n");
        for hit in &invocation.evidence {
            prompt.push_str(&format!("- [{}] {}: {}\n", hit.source_id, hit.title, hit.snippet));
        }
    }
    if !invocation.tools.is_empty() {
        let tools = invocation
            .tools
            .iter()
            .map(|tool| format!("{} ({})", tool.name, tool.kind))
            .collect::<Vec<_>>()
            .join(", ");
        prompt.push_str(&format!("\nAvailable decision tools: {tools}"));
    }
    prompt
}

fn unavailable(message: impl Into<String>) -> BrainExecutionFailure {
    BrainExecutionFailure::new(BrainExecutionFailureKind::Unavailable, message)
}

fn map_reqwest_error(error: reqwest::Error) -> BrainExecutionFailure {
    if error.is_timeout() {
        BrainExecutionFailure::new(BrainExecutionFailureKind::Timeout, "provider request timed out")
    } else {
        unavailable("provider request failed")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aionui_api_types::{DecisionToolDefinition, RoleDefinition};
    use aionui_common::encrypt_string;
    use aionui_db::{CreateProviderParams, SqliteProviderRepository, init_database_memory};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn provider(base_url: &str, full: bool) -> Provider {
        Provider {
            id: "p1".into(),
            platform: "custom".into(),
            name: "P".into(),
            base_url: base_url.into(),
            api_key_encrypted: String::new(),
            models: "[]".into(),
            enabled: true,
            capabilities: "[]".into(),
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            bedrock_config: None,
            is_full_url: full,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn builds_openai_and_anthropic_urls_without_double_suffixes() {
        assert_eq!(
            provider_url(&provider("https://api.example/v1", false), false),
            "https://api.example/v1/chat/completions"
        );
        assert_eq!(
            provider_url(&provider("https://api.anthropic.com", false), true),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            provider_url(&provider("https://proxy.example/v1", false), true),
            "https://proxy.example/v1/messages"
        );
        assert_eq!(
            provider_url(&provider("https://proxy/full", true), false),
            "https://proxy/full"
        );
    }

    #[test]
    fn rejects_non_http_userinfo_and_fragment_urls() {
        for value in [
            "file:///tmp/key",
            "https://user:secret@example.com/v1",
            "https://example.com/v1#fragment",
        ] {
            assert!(validated_provider_url(&provider(value, true), false).is_err());
        }
    }

    async fn runtime(base_url: String) -> (ProviderBrainRuntime, aionui_db::Database) {
        runtime_with_key(base_url, "secret-test-key").await
    }

    async fn runtime_with_key(base_url: String, api_key: &str) -> (ProviderBrainRuntime, aionui_db::Database) {
        let database = init_database_memory().await.unwrap();
        let repository = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        let key = [0x42; 32];
        let encrypted = encrypt_string(api_key, &key).unwrap();
        repository
            .create(CreateProviderParams {
                id: Some("provider-1"),
                platform: "local",
                name: "Provider",
                base_url: &base_url,
                api_key_encrypted: &encrypted,
                models: r#"["model-1"]"#,
                enabled: true,
                capabilities: "[]",
                context_limit: None,
                model_protocols: None,
                model_enabled: None,
                model_health: None,
                bedrock_config: None,
                is_full_url: true,
            })
            .await
            .unwrap();
        (ProviderBrainRuntime::new(repository, key), database)
    }

    fn invocation() -> BrainInvocation {
        BrainInvocation {
            decision_id: "decision-1".into(),
            session_id: "session-1".into(),
            brain: BrainDefinition {
                id: Some("brain-1".into()),
                kind: BrainKind::ProviderModel,
                provider_id: "provider-1".into(),
                model: "model-1".into(),
                role_id: "strategy".into(),
                agent_id: None,
                tool_ids: vec![],
            },
            question: "Should we ship?".into(),
            role: RoleDefinition {
                id: "strategy".into(),
                name: "Strategy".into(),
                instructions: "Assess the decision".into(),
            },
            tools: Vec::<DecisionToolDefinition>::new(),
            interjections: vec![],
            evidence: vec![],
        }
    }

    #[tokio::test]
    async fn redirect_is_not_followed_with_provider_credentials() {
        let target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "must not be reached"}}]
            })))
            .expect(0)
            .mount(&target)
            .await;
        let source = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", format!("{}/capture", target.uri())))
            .expect(1)
            .mount(&source)
            .await;
        let (runtime, _database) = runtime(format!("{}/redirect", source.uri())).await;

        let error = runtime.execute(invocation()).await.unwrap_err();
        assert_eq!(error.kind, BrainExecutionFailureKind::Unavailable);
        source.verify().await;
        target.verify().await;
    }

    #[tokio::test]
    async fn oversized_success_response_is_rejected_before_json_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_PROVIDER_RESPONSE_BYTES + 1]))
            .expect(1)
            .mount(&server)
            .await;
        let (runtime, _database) = runtime(format!("{}/large", server.uri())).await;

        let error = runtime.execute(invocation()).await.unwrap_err();
        assert_eq!(error.kind, BrainExecutionFailureKind::Unavailable);
        assert!(error.message.contains("size limit"));
        server.verify().await;
    }

    #[test]
    fn metadata_endpoint_is_rejected() {
        assert!(validated_provider_url(&provider("http://169.254.169.254/latest/meta-data", true), false).is_err());
    }

    #[tokio::test]
    async fn loopback_provider_can_run_without_a_bearer_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "local opinion"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (runtime, _database) = runtime_with_key(format!("{}/local", server.uri()), "").await;
        let opinion = runtime.execute(invocation()).await.unwrap();
        assert_eq!(opinion.content, "local opinion");
        server.verify().await;
    }
}
