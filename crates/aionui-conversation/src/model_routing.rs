use aionui_db::models::ConversationRow;
use async_trait::async_trait;

use crate::ConversationError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationModelRequirements {
    pub function_calling: bool,
    pub vision: bool,
    pub context_tokens: i64,
    pub protocol: Option<String>,
    pub run_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationModelLease {
    pub route_id: String,
    pub member_id: String,
    pub provider_id: String,
    pub model: String,
    pub fallback_used: bool,
    pub accounted: bool,
    pub run_id: String,
    pub user_id: String,
    pub acquired_at: i64,
}

#[async_trait]
pub trait ConversationModelRouteResolver: Send + Sync {
    async fn acquire(
        &self,
        route_id: &str,
        conversation_id: &str,
        requirements: ConversationModelRequirements,
        primary_waited_ms: u64,
    ) -> Result<ConversationModelLease, ConversationError>;

    async fn release(&self, lease: &ConversationModelLease, tokens: i64);

    async fn record_failure(&self, lease: &ConversationModelLease, message: &str);
}

pub fn logical_route_id(row: &ConversationRow) -> Option<String> {
    let model = row.model.as_deref()?;
    let value: serde_json::Value = serde_json::from_str(model).ok()?;
    value
        .get("provider_id")
        .or_else(|| value.get("providerId"))
        .and_then(|value| value.as_str())
        .and_then(|provider_id| provider_id.strip_prefix("route:"))
        .filter(|route_id| !route_id.is_empty())
        .map(str::to_owned)
}

pub fn apply_physical_assignment(row: &ConversationRow, lease: &ConversationModelLease) -> ConversationRow {
    let mut routed = row.clone();
    routed.model = serde_json::to_string(&aionui_common::ProviderWithModel {
        provider_id: lease.provider_id.clone(),
        model: lease.model.clone(),
        use_model: Some(lease.model.clone()),
    })
    .ok();
    routed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_route_provider_ids_are_logical() {
        let mut row = ConversationRow {
            id: "c".into(),
            user_id: "u".into(),
            name: "n".into(),
            r#type: "aionrs".into(),
            model: Some(r#"{"provider_id":"route:fast","model":"Fast"}"#.into()),
            extra: "{}".into(),
            status: None,
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(logical_route_id(&row).as_deref(), Some("fast"));
        row.model = Some(r#"{"provider_id":"physical","model":"m"}"#.into());
        assert!(logical_route_id(&row).is_none());
    }
}
