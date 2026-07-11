use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::{
    CreateDevicePairingParams, IDeviceRepository, RedeemDevicePairingParams, SqliteDeviceRepository,
    init_database_memory,
};

#[tokio::test]
async fn device_repository_is_user_scoped_and_revocation_blocks_authentication() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteDeviceRepository::new(db.pool().clone());
    let now = now_ms();
    let code_hash = "a".repeat(64);
    let token_hash = "b".repeat(64);

    repo.create_pairing(CreateDevicePairingParams {
        id: "pair-one",
        user_id: "system_default_user",
        code_hash: &code_hash,
        server_url: "http://192.168.1.5:25808",
        expires_at: now + 300_000,
        created_at: now,
    })
    .await
    .unwrap();
    let device = repo
        .redeem_pairing(RedeemDevicePairingParams {
            code_hash: &code_hash,
            device_id: "device-one",
            token_hash: &token_hash,
            name: "Phone",
            platform: "ios",
            now,
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(device.user_id, "system_default_user");
    assert_eq!(repo.list_devices("system_default_user").await.unwrap().len(), 1);
    assert!(repo.list_devices("another-user").await.unwrap().is_empty());
    assert!(repo.authenticate_token(&token_hash, now + 1).await.unwrap().is_some());
    assert!(
        repo.revoke_device("another-user", "device-one", now + 2)
            .await
            .unwrap()
            .is_none()
    );
    let revoked = repo
        .revoke_device("system_default_user", "device-one", now + 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revoked.revoked_at, Some(now + 2));
    assert!(repo.authenticate_token(&token_hash, now + 3).await.unwrap().is_none());
}

#[tokio::test]
async fn concurrent_redemption_has_exactly_one_winner() {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteDeviceRepository::new(db.pool().clone()));
    let now = now_ms();
    let code_hash = "c".repeat(64);
    repo.create_pairing(CreateDevicePairingParams {
        id: "pair-race",
        user_id: "system_default_user",
        code_hash: &code_hash,
        server_url: "http://centaur-server:25808",
        expires_at: now + 300_000,
        created_at: now,
    })
    .await
    .unwrap();

    let repo_a = repo.clone();
    let repo_b = repo.clone();
    let token_a = "d".repeat(64);
    let token_b = "e".repeat(64);
    let (first, second) = tokio::join!(
        repo_a.redeem_pairing(RedeemDevicePairingParams {
            code_hash: &code_hash,
            device_id: "device-race-a",
            token_hash: &token_a,
            name: "Phone A",
            platform: "ios",
            now,
        }),
        repo_b.redeem_pairing(RedeemDevicePairingParams {
            code_hash: &code_hash,
            device_id: "device-race-b",
            token_hash: &token_b,
            name: "Phone B",
            platform: "android",
            now,
        })
    );

    let winners = [first.unwrap(), second.unwrap()]
        .into_iter()
        .filter(Option::is_some)
        .count();
    assert_eq!(winners, 1);
    assert_eq!(repo.list_devices("system_default_user").await.unwrap().len(), 1);
}

#[tokio::test]
async fn expired_pairing_cannot_be_consumed() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteDeviceRepository::new(db.pool().clone());
    let now = now_ms();
    let code_hash = "f".repeat(64);
    repo.create_pairing(CreateDevicePairingParams {
        id: "pair-expired",
        user_id: "system_default_user",
        code_hash: &code_hash,
        server_url: "http://127.0.0.1:25808",
        expires_at: now - 1,
        created_at: now - 10,
    })
    .await
    .unwrap();

    let token_hash = "0".repeat(64);
    let result = repo
        .redeem_pairing(RedeemDevicePairingParams {
            code_hash: &code_hash,
            device_id: "device-expired",
            token_hash: &token_hash,
            name: "Expired",
            platform: "ios",
            now,
        })
        .await
        .unwrap();
    assert!(result.is_none());
}
