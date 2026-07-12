use std::collections::HashMap;
use std::net::IpAddr;
#[cfg(test)]
use std::net::SocketAddr;
use std::sync::Arc;

use aionui_api_types::{BrainDefinition, BrainKind};
use aionui_common::decrypt_string;
use aionui_common::outbound::{OutboundHttpPolicy, is_numeric_loopback, resolve_pinned_http_endpoint};
#[cfg(test)]
use aionui_common::outbound::{validate_http_endpoint, validate_resolved_addresses as validate_outbound_addresses};
use aionui_db::{IProviderRepository, models::Provider};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::ports::{
    BrainCatalogPort, BrainExecutionFailure, BrainExecutionFailureKind, BrainExecutionPlan, BrainExecutionPort,
    BrainInvocation, BrainLocation, BrainOpinion,
};

/// Executes provider-model brains without exposing decrypted credentials to a client.
///
/// OpenAI-compatible and Anthropic-compatible providers are supported directly.
/// ACP brains remain part of the public domain model and can be supplied by a
/// composite executor without weakening this provider credential boundary.
pub struct ProviderBrainRuntime {
    providers: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
}

const MAX_PROVIDER_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const PROVIDER_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PROVIDER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(50);

impl ProviderBrainRuntime {
    pub fn new(providers: Arc<dyn IProviderRepository>, encryption_key: [u8; 32]) -> Self {
        Self {
            providers,
            encryption_key,
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
    async fn prepare(&self, brain: &BrainDefinition) -> Result<BrainExecutionPlan, BrainExecutionFailure> {
        if brain.kind != BrainKind::ProviderModel {
            return Err(BrainExecutionFailure::new(
                BrainExecutionFailureKind::Unsupported,
                "ACP brain requires an ACP execution adapter",
            ));
        }

        let provider = self.provider(&brain.provider_id).await?;
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
        let protocol = provider_protocol(&provider, &brain.model);
        let endpoint = resolve_provider_endpoint(&provider, &protocol).await?;
        let address = endpoint.url.host_str().and_then(parse_host_ip);
        let location = if is_local_platform(&provider.platform) && address.is_some_and(is_numeric_loopback) {
            BrainLocation::Local
        } else {
            BrainLocation::External
        };
        Ok(BrainExecutionPlan::new(
            brain.clone(),
            location,
            ProviderExecutionSnapshot {
                protocol,
                api_key,
                endpoint,
            },
        ))
    }

    async fn execute(
        &self,
        plan: BrainExecutionPlan,
        invocation: BrainInvocation,
    ) -> Result<BrainOpinion, BrainExecutionFailure> {
        if invocation.brain != plan.brain {
            return Err(unavailable("prepared Brain does not match its invocation"));
        }
        let snapshot = plan
            .payload::<ProviderExecutionSnapshot>()
            .ok_or_else(|| unavailable("provider execution snapshot is invalid"))?;
        let prompt = build_prompt(&invocation);
        let client = &snapshot.endpoint.client;
        let url = snapshot.endpoint.url.clone();

        let request = if snapshot.protocol == "anthropic" {
            let request = client.post(url).header("anthropic-version", "2023-06-01").json(&json!({
                "model": invocation.brain.model,
                "max_tokens": 2048,
                "system": invocation.role.instructions,
                "messages": [{"role": "user", "content": prompt}],
            }));
            if let Some(api_key) = snapshot.api_key.as_deref() {
                request.header("x-api-key", api_key)
            } else {
                request
            }
        } else {
            let request = client.post(url).json(&json!({
                "model": invocation.brain.model,
                "messages": [
                    {"role": "system", "content": invocation.role.instructions},
                    {"role": "user", "content": prompt}
                ],
                "temperature": 0.2
            }));
            if let Some(api_key) = snapshot.api_key.as_deref() {
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
        let content = if snapshot.protocol == "anthropic" {
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
            } else if provider.platform.eq_ignore_ascii_case("gemini") {
                "gemini".to_owned()
            } else {
                "openai".to_owned()
            }
        })
}

fn provider_url(provider: &Provider, protocol: &str) -> String {
    let base = provider.base_url.trim_end_matches('/');
    if provider.is_full_url {
        return base.to_owned();
    }
    if protocol == "anthropic" {
        if base.ends_with("/v1/messages") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    } else if protocol == "gemini" || provider.platform.eq_ignore_ascii_case("gemini") {
        if base.ends_with("/v1beta/openai/chat/completions") {
            base.to_owned()
        } else if base.ends_with("/v1beta/openai") {
            format!("{base}/chat/completions")
        } else if base.ends_with("/v1beta") {
            format!("{base}/openai/chat/completions")
        } else {
            format!("{base}/v1beta/openai/chat/completions")
        }
    } else if base.ends_with("/chat/completions") {
        base.to_owned()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

struct ResolvedProviderEndpoint {
    url: reqwest::Url,
    client: reqwest::Client,
}

struct ProviderExecutionSnapshot {
    protocol: String,
    api_key: Option<String>,
    endpoint: ResolvedProviderEndpoint,
}

#[cfg(test)]
fn validated_provider_url(provider: &Provider, protocol: &str) -> Result<reqwest::Url, BrainExecutionFailure> {
    let policy = provider_outbound_policy(provider);
    let url = validate_http_endpoint(&provider_url(provider, protocol), policy)
        .map_err(|_| unavailable("provider URL violates the HTTP endpoint policy"))?;
    if url.query().is_some() {
        return Err(unavailable("provider URL violates the HTTP endpoint policy"));
    }
    Ok(url)
}

async fn resolve_provider_endpoint(
    provider: &Provider,
    protocol: &str,
) -> Result<ResolvedProviderEndpoint, BrainExecutionFailure> {
    let raw_url = provider_url(provider, protocol);
    let endpoint = resolve_pinned_http_endpoint(
        &raw_url,
        provider_outbound_policy(provider),
        PROVIDER_CONNECT_TIMEOUT,
        PROVIDER_REQUEST_TIMEOUT,
    )
    .await
    .map_err(|_| unavailable("provider endpoint failed outbound policy validation"))?;
    if endpoint.url.query().is_some() {
        return Err(unavailable("provider URL violates the HTTP endpoint policy"));
    }
    Ok(ResolvedProviderEndpoint {
        url: endpoint.url,
        client: endpoint.client,
    })
}

#[cfg(test)]
fn validate_resolved_addresses(provider: &Provider, addresses: &[SocketAddr]) -> Result<(), BrainExecutionFailure> {
    validate_outbound_addresses(provider_outbound_policy(provider), addresses)
        .map_err(|_| unavailable("provider host resolved to a forbidden address"))
}

fn provider_outbound_policy(provider: &Provider) -> OutboundHttpPolicy {
    if is_local_platform(&provider.platform) {
        OutboundHttpPolicy::NumericLoopback
    } else {
        OutboundHttpPolicy::PublicHttps
    }
}

fn is_local_platform(platform: &str) -> bool {
    matches!(
        platform.trim().to_ascii_lowercase().as_str(),
        "ollama" | "local" | "llama.cpp" | "llamacpp"
    )
}

fn parse_host_ip(host: &str) -> Option<IpAddr> {
    host.trim_matches(|character| matches!(character, '[' | ']'))
        .parse()
        .ok()
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
    use wiremock::matchers::{header, method, path};
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
            provider_url(&provider("https://api.example/v1", false), "openai"),
            "https://api.example/v1/chat/completions"
        );
        assert_eq!(
            provider_url(&provider("https://api.anthropic.com", false), "anthropic"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            provider_url(&provider("https://proxy.example/v1", false), "anthropic"),
            "https://proxy.example/v1/messages"
        );
        assert_eq!(
            provider_url(&provider("https://proxy/full", true), "openai"),
            "https://proxy/full"
        );
    }

    #[test]
    fn builds_first_launch_provider_template_urls() {
        let mut openai = provider("https://api.openai.com", false);
        openai.platform = "openai".into();
        assert_eq!(
            provider_url(&openai, &provider_protocol(&openai, "gpt-5")),
            "https://api.openai.com/v1/chat/completions"
        );

        let mut anthropic = provider("https://api.anthropic.com", false);
        anthropic.platform = "anthropic".into();
        assert_eq!(
            provider_url(&anthropic, &provider_protocol(&anthropic, "claude-sonnet-4-5")),
            "https://api.anthropic.com/v1/messages"
        );

        let mut gemini = provider("https://generativelanguage.googleapis.com", false);
        gemini.platform = "gemini".into();
        assert_eq!(
            provider_url(&gemini, &provider_protocol(&gemini, "gemini-2.5-pro")),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );

        let mut ollama = provider("http://127.0.0.1:11434", false);
        ollama.platform = "ollama".into();
        assert_eq!(
            provider_url(&ollama, &provider_protocol(&ollama, "qwen3")),
            "http://127.0.0.1:11434/v1/chat/completions"
        );

        let mut tokenclub = provider("https://tokenclub.example", false);
        tokenclub.platform = "new-api".into();
        assert_eq!(
            provider_url(&tokenclub, &provider_protocol(&tokenclub, "gpt-5")),
            "https://tokenclub.example/v1/chat/completions"
        );
    }

    #[test]
    fn rejects_non_http_userinfo_and_fragment_urls() {
        for value in [
            "file:///tmp/key",
            "https://user:secret@example.com/v1",
            "https://example.com/v1#fragment",
        ] {
            assert!(validated_provider_url(&provider(value, true), "openai").is_err());
        }
    }

    async fn runtime(base_url: String) -> (ProviderBrainRuntime, aionui_db::Database) {
        runtime_with_key(base_url, "secret-test-key").await
    }

    async fn runtime_with_key(base_url: String, api_key: &str) -> (ProviderBrainRuntime, aionui_db::Database) {
        runtime_with_provider(base_url, api_key, "local", None, true).await
    }

    async fn runtime_with_provider(
        base_url: String,
        api_key: &str,
        platform: &str,
        model_protocols: Option<&str>,
        is_full_url: bool,
    ) -> (ProviderBrainRuntime, aionui_db::Database) {
        let database = init_database_memory().await.unwrap();
        let repository = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        let key = [0x42; 32];
        let encrypted = encrypt_string(api_key, &key).unwrap();
        repository
            .create(CreateProviderParams {
                id: Some("provider-1"),
                platform,
                name: "Provider",
                base_url: &base_url,
                api_key_encrypted: &encrypted,
                models: r#"["model-1"]"#,
                enabled: true,
                capabilities: "[]",
                context_limit: None,
                model_protocols,
                model_enabled: None,
                model_health: None,
                bedrock_config: None,
                is_full_url,
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

    async fn execute_prepared(
        runtime: &ProviderBrainRuntime,
        invocation: BrainInvocation,
    ) -> Result<BrainOpinion, BrainExecutionFailure> {
        let plan = runtime.prepare(&invocation.brain).await?;
        runtime.execute(plan, invocation).await
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

        let error = execute_prepared(&runtime, invocation()).await.unwrap_err();
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

        let error = execute_prepared(&runtime, invocation()).await.unwrap_err();
        assert_eq!(error.kind, BrainExecutionFailureKind::Unavailable);
        assert!(error.message.contains("size limit"));
        server.verify().await;
    }

    #[tokio::test]
    async fn gemini_byok_uses_openai_compatible_endpoint_and_bearer_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/openai/chat/completions"))
            .and(header("authorization", "Bearer gemini-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "gemini opinion"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        // A loopback provider keeps this test hermetic. The explicit model
        // protocol exercises the same Gemini OpenAI-compatible wire contract
        // used by the external Gemini preset.
        let (runtime, _database) = runtime_with_provider(
            server.uri(),
            "gemini-test-key",
            "local",
            Some(r#"{"model-1":"gemini"}"#),
            false,
        )
        .await;

        let opinion = execute_prepared(&runtime, invocation()).await.unwrap();
        assert_eq!(opinion.content, "gemini opinion");
        server.verify().await;
    }

    #[test]
    fn metadata_endpoint_is_rejected() {
        assert!(validated_provider_url(&provider("http://169.254.169.254/latest/meta-data", true), "openai").is_err());
    }

    #[test]
    fn resolved_dns_policy_is_fail_closed_for_dual_stack_and_rebinding_answers() {
        let external = provider("https://api.example.test/v1", true);
        let public_dual_stack = [
            "8.8.8.8:443".parse::<SocketAddr>().unwrap(),
            "[2606:4700:4700::1111]:443".parse::<SocketAddr>().unwrap(),
        ];
        assert!(validate_resolved_addresses(&external, &public_dual_stack).is_ok());

        for poisoned in [
            "127.0.0.1:443",
            "10.0.0.1:443",
            "169.254.169.254:443",
            "[::1]:443",
            "[fc00::1]:443",
            "[fe80::1]:443",
        ] {
            let answers = [
                "8.8.8.8:443".parse::<SocketAddr>().unwrap(),
                poisoned.parse::<SocketAddr>().unwrap(),
            ];
            assert!(validate_resolved_addresses(&external, &answers).is_err(), "{poisoned}");
        }
    }

    #[test]
    fn local_provider_requires_numeric_loopback_and_never_resolves_a_hostname() {
        let mut local = provider("http://127.0.0.1:11434/v1", true);
        local.platform = "ollama".into();
        assert!(validated_provider_url(&local, "openai").is_ok());
        local.base_url = "http://[::1]:11434/v1".into();
        assert!(validated_provider_url(&local, "openai").is_ok());
        local.base_url = "http://localhost:11434/v1".into();
        assert!(validated_provider_url(&local, "openai").is_err());
        local.base_url = "http://192.168.1.5:11434/v1".into();
        assert!(validated_provider_url(&local, "openai").is_err());
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
        let opinion = execute_prepared(&runtime, invocation()).await.unwrap();
        assert_eq!(opinion.content, "local opinion");
        server.verify().await;
    }

    #[tokio::test]
    async fn prepared_attempt_is_immutable_across_provider_configuration_updates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "snapshot opinion"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (runtime, database) = runtime(format!("{}/snapshot", server.uri())).await;
        let invocation = invocation();
        let plan = runtime.prepare(&invocation.brain).await.unwrap();
        assert_eq!(plan.location, BrainLocation::Local);

        sqlx::query(
            "UPDATE providers SET platform = 'openai', base_url = 'https://api.invalid.example/v1' \
             WHERE id = 'provider-1'",
        )
        .execute(database.pool())
        .await
        .unwrap();

        let opinion = runtime.execute(plan, invocation).await.unwrap();
        assert_eq!(opinion.content, "snapshot opinion");
        server.verify().await;
    }
}
