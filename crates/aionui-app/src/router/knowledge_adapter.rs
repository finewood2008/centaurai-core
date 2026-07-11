use std::net::IpAddr;
use std::sync::Arc;

use aionui_conversation::ConversationService;
use aionui_db::IProviderRepository;
use aionui_knowledge::{ModelLocation, ModelLocationResolver};
use async_trait::async_trait;

pub(crate) struct AppModelLocationResolver {
    conversations: ConversationService,
    providers: Arc<dyn IProviderRepository>,
}

impl AppModelLocationResolver {
    pub(crate) fn new(conversations: ConversationService, providers: Arc<dyn IProviderRepository>) -> Self {
        Self {
            conversations,
            providers,
        }
    }
}

#[async_trait]
impl ModelLocationResolver for AppModelLocationResolver {
    async fn resolve(&self, user_id: &str, conversation_id: &str) -> ModelLocation {
        let Ok(conversation) = self.conversations.get(user_id, conversation_id).await else {
            return ModelLocation::Unknown;
        };
        let Some(model) = conversation.model else {
            return ModelLocation::Unknown;
        };
        let Ok(Some(provider)) = self.providers.find_by_id(&model.provider_id).await else {
            return ModelLocation::Unknown;
        };
        if is_local_provider(&provider.platform, &provider.base_url) {
            ModelLocation::Local
        } else {
            ModelLocation::External
        }
    }
}

fn is_local_provider(platform: &str, base_url: &str) -> bool {
    let explicitly_local = matches!(
        platform.trim().to_ascii_lowercase().as_str(),
        "ollama" | "local" | "llama.cpp" | "llamacpp"
    );
    if !explicitly_local {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host.parse::<IpAddr>().is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_local_provider_targets_are_local() {
        assert!(is_local_provider("ollama", "http://127.0.0.1:11434"));
        assert!(is_local_provider("local", "http://localhost:11434/v1"));
        assert!(!is_local_provider("ollama", "http://example.com"));
        assert!(!is_local_provider("openai", "http://127.0.0.1:11434/v1"));
        assert!(!is_local_provider("openai", "https://api.openai.com/v1"));
        assert!(!is_local_provider("openai", "not-a-url"));
    }
}
