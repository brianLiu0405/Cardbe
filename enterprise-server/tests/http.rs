use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use cardbe_enterprise_server::{
    http::{router, AppState},
    store::Store,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

fn app(store: Store) -> Router {
    router(AppState::new(
        store,
        9040,
        vec!["http://127.0.0.1:1441".into()],
    ))
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:9040")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn create_board(app: &Router, token: &str) -> String {
    let (status, workspace) = request(
        app,
        "POST",
        "/v1/workspaces",
        Some(token),
        json!({"name":"工作區"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{workspace}");
    let (status, board) = request(
        app,
        "POST",
        "/v1/boards",
        Some(token),
        json!({"name":"共同看板","workspace_id":workspace["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{board}");
    board["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn boundary_requires_token_host_and_allowed_origin_and_redacts_errors() {
    let mut store = Store::open(":memory:").unwrap();
    let bootstrap = store.bootstrap("Org", "Owner", "Device").unwrap();
    let app = app(store);
    assert_eq!(
        request(&app, "GET", "/v1/me", None, Value::Null).await,
        (StatusCode::UNAUTHORIZED, json!({"code":"UNAUTHENTICATED"}))
    );
    assert_eq!(
        request(&app, "GET", "/v1/me", Some("invalid"), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for (host, origin) in [
        ("attacker.invalid", "http://127.0.0.1:1441"),
        ("127.0.0.1:9040", "https://attacker.invalid"),
        ("127.0.0.1:9040", "null"),
    ] {
        let request = Request::builder()
            .uri("/v1/me")
            .header("host", host)
            .header("origin", origin)
            .header(
                "authorization",
                format!("Bearer {}", bootstrap["token"].as_str().unwrap()),
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let (status, health) = request(&app, "GET", "/health", None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["mode"], "prototype");
    assert_eq!(health["storage"], "sqlite");
}

#[tokio::test]
async fn duplicate_boundary_headers_are_rejected() {
    let app = app(Store::open(":memory:").unwrap());
    for duplicate in ["host", "origin"] {
        let mut request = Request::builder()
            .uri("/health")
            .header("host", "127.0.0.1:9040")
            .header("origin", "http://127.0.0.1:1441")
            .body(Body::empty())
            .unwrap();
        request
            .headers_mut()
            .append(duplicate, "https://attacker.invalid".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn tenant_isolation_viewer_denial_and_revocation_apply_to_replays() {
    let mut store = Store::open(":memory:").unwrap();
    let owner = store.bootstrap("甲公司", "Owner", "Device").unwrap();
    let outsider = store.bootstrap("乙公司", "Other owner", "Device").unwrap();
    let owner_token = owner["token"].as_str().unwrap();
    let app = app(store);
    let board_id = create_board(&app, owner_token).await;
    let (status, member) = request(
        &app,
        "POST",
        "/v1/members",
        Some(owner_token),
        json!({"name":"Team member","role":"member","device_name":"Laptop"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{member}");
    let member_token = member["token"].as_str().unwrap();
    let access_path = format!("/v1/boards/{board_id}/access");
    let command_path = format!("/v1/boards/{board_id}/commands");
    let snapshot_path = format!("/v1/boards/{board_id}/snapshot");
    assert_eq!(
        request(
            &app,
            "PUT",
            &access_path,
            Some(owner_token),
            json!({"member_id":member["member"]["id"],"role":"viewer"})
        )
        .await
        .0,
        StatusCode::OK
    );
    // Omitted role is malformed; only explicit null may revoke an access grant.
    assert_eq!(
        request(
            &app,
            "PUT",
            &access_path,
            Some(owner_token),
            json!({"member_id":member["member"]["id"]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let command = json!({"operation_id":Uuid::new_v4().to_string(),"command":{"type":"task.create","title":"離線重送測試"}});
    assert_eq!(
        request(
            &app,
            "POST",
            &command_path,
            Some(member_token),
            command.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&app, "GET", &snapshot_path, Some(member_token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(matches!(
        request(
            &app,
            "GET",
            &snapshot_path,
            outsider["token"].as_str(),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));
    assert_eq!(
        request(
            &app,
            "PUT",
            &access_path,
            Some(owner_token),
            json!({"member_id":member["member"]["id"],"role":"editor"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, first) = request(
        &app,
        "POST",
        &command_path,
        Some(member_token),
        command.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["replayed"], false);
    let (status, replay) = request(
        &app,
        "POST",
        &command_path,
        Some(member_token),
        command.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["replayed"], true);
    assert_eq!(first["result"], replay["result"]);
    let mut changed = command.clone();
    changed["command"]["title"] = json!("different payload");
    assert_eq!(
        request(&app, "POST", &command_path, Some(member_token), changed).await,
        (StatusCode::CONFLICT, json!({"code":"IDEMPOTENCY_CONFLICT"}))
    );
    // Current board permission is checked before looking up an accepted receipt.
    assert_eq!(
        request(
            &app,
            "PUT",
            &access_path,
            Some(owner_token),
            json!({"member_id":member["member"]["id"],"role":null})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(matches!(
        request(
            &app,
            "POST",
            &command_path,
            Some(member_token),
            command.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));
    let revoke_path = format!("/v1/devices/{}", member["device"]["id"].as_str().unwrap());
    assert_eq!(
        request(&app, "DELETE", &revoke_path, Some(owner_token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/v1/me", Some(member_token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, audit) = request(
        &app,
        "GET",
        "/v1/audit?limit=100",
        Some(owner_token),
        Value::Null,
    )
    .await;
    assert!(audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["operation_id"] == command["operation_id"]));
    assert!(audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["action"] == "member.create" && e["resource_id"] == member["member"]["id"]));
    assert!(!audit.to_string().contains(member_token));
}

#[tokio::test]
async fn invalid_payloads_cannot_supply_identity_or_escalate_roles() {
    let mut store = Store::open(":memory:").unwrap();
    let bootstrap = store.bootstrap("Org", "Owner", "Device").unwrap();
    let token = bootstrap["token"].as_str().unwrap();
    let app = app(store);
    let (status, body) = request(
        &app,
        "POST",
        "/v1/workspaces",
        Some(token),
        json!({"name":"test","actor_id":"forged","role":"owner"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (StatusCode::BAD_REQUEST, json!({"code":"INVALID_ARGUMENT"}))
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/v1/members",
            Some(token),
            json!({"name":"Forged owner","role":"owner","device_name":"Device"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/v1/audit?after=invalid",
            Some(token),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/v1/boards?org_id=other",
            Some(token),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/workspaces")
        .header("host", "127.0.0.1:9040")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from("{"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 1024).await.unwrap())
            .unwrap(),
        json!({"code":"INVALID_ARGUMENT"})
    );
}

#[tokio::test]
async fn accepted_operation_survives_server_restart_without_duplicate_task() {
    let directory = std::env::temp_dir().join(format!("cardbe-enterprise-http-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("test.sqlite3");
    let mut store = Store::open(&path).unwrap();
    let bootstrap = store.bootstrap("Org", "Owner", "Device").unwrap();
    let token = bootstrap["token"].as_str().unwrap();
    let first_app = app(store);
    let board_id = create_board(&first_app, token).await;
    let command_path = format!("/v1/boards/{board_id}/commands");
    let command = json!({"operation_id":Uuid::new_v4().to_string(),"command":{"type":"task.create","title":"Persist 中文 ✓"}});
    let (status, first) = request(
        &first_app,
        "POST",
        &command_path,
        Some(token),
        command.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    drop(first_app);
    let second_app = app(Store::open(&path).unwrap());
    let (status, replayed) =
        request(&second_app, "POST", &command_path, Some(token), command).await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["replayed"], true);
    assert_eq!(first["result"], replayed["result"]);
    let (_, snapshot) = request(
        &second_app,
        "GET",
        &format!("/v1/boards/{board_id}/snapshot"),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(snapshot["tasks"].as_array().unwrap().len(), 1);
    drop(second_app);
    std::fs::remove_dir_all(directory).unwrap();
}
