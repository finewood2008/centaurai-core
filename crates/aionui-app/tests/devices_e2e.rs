use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_app::{AppConfig, AppServices};

async fn build_app() -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn bearer_json(method: &str, uri: &str, body: serde_json::Value, token: &str) -> Request<Body> {
    let mut request = json_request(method, uri, body);
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    request
}

fn pairing_code(pairing_uri: &str) -> String {
    pairing_uri.rsplit("code=").next().unwrap().to_owned()
}

async fn setup_owner(app: &axum::Router, services: &AppServices) -> (String, String) {
    let password = "StrongP@ss1";
    let hash = aionui_auth::hash_password(password).unwrap();
    services
        .user_repo
        .set_system_user_credentials("admin", &hash)
        .await
        .unwrap();

    let csrf_response = app
        .clone()
        .oneshot(Request::get("/api/auth/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let csrf = csrf_response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            cookie
                .strip_prefix("centaurai-csrf-token=")
                .and_then(|value| value.split(';').next())
                .map(str::to_owned)
        })
        .unwrap();

    let login = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/login",
            serde_json::json!({"username": "admin", "password": password}),
        ))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let token = body_json(login).await["token"].as_str().unwrap().to_owned();
    (token, csrf)
}

#[tokio::test]
async fn pairing_creation_enforces_cookie_csrf_and_authentication() {
    let (app, services) = build_app().await;
    let (token, csrf) = setup_owner(&app, &services).await;

    let anonymous = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/devices/pairing",
            serde_json::json!({"server_url": "http://192.168.1.20:25808"}),
        ))
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let mut cross_origin = json_request(
        "POST",
        "/api/devices/pairing",
        serde_json::json!({"server_url": "http://192.168.1.20:25808"}),
    );
    cross_origin.headers_mut().insert(
        header::COOKIE,
        format!("centaurai-session={token}; centaurai-csrf-token={csrf}")
            .parse()
            .unwrap(),
    );
    cross_origin
        .headers_mut()
        .insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
    let response = app.clone().oneshot(cross_origin).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");

    let mut valid_cookie = json_request(
        "POST",
        "/api/devices/pairing",
        serde_json::json!({"server_url": "http://192.168.1.20:25808"}),
    );
    valid_cookie.headers_mut().insert(
        header::COOKIE,
        format!("centaurai-session={token}; centaurai-csrf-token={csrf}")
            .parse()
            .unwrap(),
    );
    valid_cookie.headers_mut().insert("x-csrf-token", csrf.parse().unwrap());
    let response = app.clone().oneshot(valid_cookie).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let mut forged_bearer = bearer_json(
        "POST",
        "/api/devices/pairing",
        serde_json::json!({"server_url": "http://192.168.1.20:25808"}),
        "forged",
    );
    forged_bearer
        .headers_mut()
        .insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
    let response = app.oneshot(forged_bearer).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_device_flow_needs_no_csrf_and_redeem_ignores_stale_cookies() {
    let (app, services) = build_app().await;
    let (owner_token, _csrf) = setup_owner(&app, &services).await;

    let mut create = bearer_json(
        "POST",
        "/api/devices/pairing",
        serde_json::json!({"server_url": "https://context.home.local"}),
        &owner_token,
    );
    create
        .headers_mut()
        .insert(header::ORIGIN, "https://native-client.invalid".parse().unwrap());
    let response = app.clone().oneshot(create).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let pairing = body_json(response).await;
    let code = pairing_code(pairing["data"]["pairing_uri"].as_str().unwrap());

    let mut redeem = json_request(
        "POST",
        "/api/devices/pairing/redeem",
        serde_json::json!({"code": code, "name": "Phone", "platform": "ios"}),
    );
    redeem
        .headers_mut()
        .insert(header::COOKIE, "centaurai-session=stale".parse().unwrap());
    let response = app.clone().oneshot(redeem).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let credential = body_json(response).await;
    let device_id = credential["data"]["device"]["id"].as_str().unwrap().to_owned();
    let device_token = credential["data"]["device_token"].as_str().unwrap().to_owned();
    assert!(device_token.starts_with("cai_dev_v1_"));
    assert!(credential["data"]["device"].get("token_hash").is_none());

    let list = app
        .clone()
        .oneshot(
            Request::get("/api/devices")
                .header("authorization", format!("Bearer {device_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);

    let revoke = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            &format!("/api/devices/{device_id}/revoke"),
            serde_json::json!({}),
            &owner_token,
        ))
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::OK);

    let rejected = app
        .oneshot(
            Request::get("/api/devices")
                .header("authorization", format!("Bearer {device_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(rejected).await["code"], "UNAUTHORIZED");
}
