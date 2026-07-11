use serde::{Deserialize, Serialize};

/// Public, non-secret device metadata aligned with the Context SDK contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceResponse {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CreateDevicePairingRequest {
    pub server_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevicePairingSessionResponse {
    pub id: String,
    pub pairing_uri: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RedeemDevicePairingRequest {
    pub code: String,
    pub name: String,
    pub platform: String,
}

/// The device token is returned exactly once when a pairing is redeemed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedDeviceCredentialResponse {
    pub device: DeviceResponse,
    pub device_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_shapes_match_context_sdk() {
        let device = DeviceResponse {
            id: "device-one".into(),
            name: "Phone".into(),
            platform: "ios".into(),
            last_seen_at: None,
            created_at: "2026-07-12T00:00:00.000Z".into(),
            revoked_at: None,
        };
        let credential = PairedDeviceCredentialResponse {
            device,
            device_token: "cai_dev_v1_secret".into(),
        };
        let json = serde_json::to_value(credential).unwrap();
        assert_eq!(json["device"]["id"], "device-one");
        assert_eq!(json["device"]["last_seen_at"], serde_json::Value::Null);
        assert_eq!(json["device_token"], "cai_dev_v1_secret");
        assert!(json.get("token_hash").is_none());
        assert!(json["device"].get("token_hash").is_none());
    }
}
