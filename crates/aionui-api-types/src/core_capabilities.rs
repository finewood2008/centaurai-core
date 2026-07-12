use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Versioned contracts exposed by `GET /api/capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreContractVersions {
    pub rest: String,
    pub websocket: String,
    pub startup: String,
}

/// WebSocket protocol version and the event names covered by that version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreWebSocketCapabilities {
    pub version: String,
    pub events: Vec<String>,
}

/// Stable client-negotiation payload returned by `GET /api/capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreCapabilitiesResponse {
    pub contract: CoreContractVersions,
    pub feature_version: String,
    pub features: BTreeMap<String, bool>,
    pub websocket: CoreWebSocketCapabilities,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_the_client_negotiation_shape() {
        let value = serde_json::to_value(CoreCapabilitiesResponse {
            contract: CoreContractVersions {
                rest: "2".into(),
                websocket: "2".into(),
                startup: "2".into(),
            },
            feature_version: "2".into(),
            features: BTreeMap::from([("agent_management_refresh".into(), true)]),
            websocket: CoreWebSocketCapabilities {
                version: "2".into(),
                events: vec!["team.agentStatusChanged".into()],
            },
        })
        .unwrap();

        assert_eq!(value["contract"]["rest"], "2");
        assert_eq!(value["contract"]["websocket"], "2");
        assert_eq!(value["feature_version"], "2");
        assert_eq!(value["features"]["agent_management_refresh"], true);
        assert_eq!(value["websocket"]["version"], "2");
        assert_eq!(value["websocket"]["events"][0], "team.agentStatusChanged");
    }
}
