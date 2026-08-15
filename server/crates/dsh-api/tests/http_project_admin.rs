//! 项目管理员（Project Admin）HTTP 集成测试（设计 §8 M2）。
//! 覆盖：登录（username 分支/防枚举/409）、授权矩阵逐行、路径绕过、token 负形、
//! reveal 越权（B2 回归）、会话生命周期（改密/删号/force-logout/heartbeat）、
//! 列表过滤（projects/audit/refs）、审计 operator。

use std::sync::{Arc, Mutex};

use dsh_api::{build_router, ApiState};
use dsh_core::command::Command;
use dsh_core::model::ProjectId;
use dsh_core::{InMemoryStore, StateMachine, token_hash};
use dsh_watch::WatchHub;

struct TestServer {
    base: String,
    _state: ApiState,
}

async fn start() -> TestServer {
    let sm = Arc::new(Mutex::new(StateMachine::new(Box::new(InMemoryStore::new()))));
    {
        let mut g = sm.lock().unwrap();
        // 两个项目 + PA 账号（p1: alice，p2 无 PA）
        for name in ["p1", "p2"] {
            g.apply(
                &Command::ProjectCreate {
                    name: name.to_string(),
                    operator: String::new(),
                },
                1,
            )
            .unwrap();
        }
        g.apply(
            &Command::ProjectAdminCreate {
                project: ProjectId("p1".into()),
                username: "alice".into(),
                salt: "s1".into(),
                password_hash: token_hash("s1alicepw"),
            },
            2,
        )
        .unwrap();
    }
    let state = ApiState::new(
        sm,
        WatchHub::new(),
        None,
        None,
        None,
        std::time::Duration::from_secs(86400),
        "admin-pw".into(),
        None,
    );
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestServer {
        base: format!("http://{addr}"),
        _state: state,
    }
}

async fn req(
    base: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut r = client.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{base}{path}"));
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, json)
}

async fn admin_login(base: &str) -> String {
    let (code, body) = req(base, "POST", "/api/v1/login", None, Some(serde_json::json!({"password": "admin-pw"}))).await;
    assert_eq!(code, 200, "admin login: {body}");
    body["token"].as_str().unwrap().to_string()
}

async fn pa_login(base: &str, user: &str, pw: &str) -> (u16, serde_json::Value) {
    req(base, "POST", "/api/v1/login", None, Some(serde_json::json!({"username": user, "password": pw}))).await
}

/// 初始化 p1 结构（admin token）。
async fn setup_p1_structure(base: &str, admin: &str) {
    let (code, body) = req(base, "PUT", "/api/v1/projects/p1/structure-draft", Some(admin), Some(serde_json::json!({
        "base_version": 1,
        "groups": [{"name": "g", "items": [{"key": "k", "type": "string", "required": true}]}]
    }))).await;
    assert_eq!(code, 200, "structure-draft: {body}");
    let (code, body) = req(base, "POST", "/api/v1/projects/p1/structure-draft/publish", Some(admin), Some(serde_json::json!({"comment": "init"}))).await;
    assert_eq!(code, 200, "structure publish: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pa_login_flow_and_role_response() {
    let s = start().await;
    // 正确账密 → 200 + role/project + pa. 前缀 token
    let (code, body) = pa_login(&s.base, "alice", "alicepw").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["role"], "project_admin");
    assert_eq!(body["project"], "p1");
    let token = body["token"].as_str().unwrap();
    assert!(token.starts_with("pa.alice."), "token 前缀路由格式: {token}");

    // 重复登录 → 409 ERR_SESSION_IN_USE（每账号单会话）
    let (code, body) = pa_login(&s.base, "alice", "alicepw").await;
    assert_eq!(code, 409);
    assert_eq!(body["code"], "ERR_SESSION_IN_USE");

    // 登出后可重登
    let (code, _) = req(&s.base, "POST", "/api/v1/logout", Some(token), None).await;
    assert_eq!(code, 204);
    let (code, _) = pa_login(&s.base, "alice", "alicepw").await;
    assert_eq!(code, 200);

    // 错误密码与不存在账号 → 同码同文案（防枚举）
    let (c1, b1) = pa_login(&s.base, "alice", "wrong").await;
    let (c2, b2) = pa_login(&s.base, "ghost", "whatever").await;
    assert_eq!((c1, b1["code"].clone()), (c2, b2["code"].clone()));
    assert_eq!(c1, 401);
    assert_eq!(b1["code"], "ERR_BAD_CREDENTIALS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pa_authorization_matrix() {
    let s = start().await;
    let admin = admin_login(&s.base).await;
    setup_p1_structure(&s.base, &admin).await;
    let (_, body) = pa_login(&s.base, "alice", "alicepw").await;
    let pa = body["token"].as_str().unwrap().to_string();

    // ✅ 自身项目：结构/值/发布/查询全通
    let (c, b) = req(&s.base, "PUT", "/api/v1/projects/p1/branches/dev/draft", Some(&pa), Some(serde_json::json!({
        "updates": [{"group": "g", "key": "k", "value": {"type": "string", "str_value": "v1"}}], "deletes": []
    }))).await;
    assert_eq!(c, 200, "PA 写自己项目草稿: {b}");
    let (c, b) = req(&s.base, "POST", "/api/v1/projects/p1/branches/dev/publish", Some(&pa), Some(serde_json::json!({"comment": "pa"}))).await;
    assert_eq!(c, 200, "PA 发布: {b}");
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1/branches/dev/config", Some(&pa), None).await;
    assert_eq!(c, 200);
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1/branches/dev/versions", Some(&pa), None).await;
    assert_eq!(c, 200);
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1/diff?branch_a=dev&branch_b=test", Some(&pa), None).await;
    assert_eq!(c, 200);
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1", Some(&pa), None).await;
    assert_eq!(c, 200);
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1/structure-draft", Some(&pa), None).await;
    assert_eq!(c, 200);
    // heartbeat 可用（B5/B7）
    let (c, _) = req(&s.base, "POST", "/api/v1/heartbeat", Some(&pa), None).await;
    assert_eq!(c, 200);

    // ❌ 跨项目（p2 属其他）
    for (m, p) in [
        ("GET", "/api/v1/projects/p2"),
        ("PUT", "/api/v1/projects/p2/branches/dev/draft"),
        ("POST", "/api/v1/projects/p2/branches/dev/publish"),
        ("GET", "/api/v1/projects/p2/branches/dev/config"),
    ] {
        let (c, b) = req(&s.base, m, p, Some(&pa), Some(serde_json::json!({}))).await;
        assert_eq!(c, 403, "{m} {p}: {b}");
    }

    // ❌ 共享面全组（含 GET）
    for (m, p) in [
        ("POST", "/api/v1/shared"),
        ("PUT", "/api/v1/shared-draft"),
        ("POST", "/api/v1/shared/publish"),
        ("GET", "/api/v1/shared"),
        ("GET", "/api/v1/shared-draft"),
        ("POST", "/api/v1/shared/refs"),
        ("DELETE", "/api/v1/shared/refs"),
    ] {
        let (c, b) = req(&s.base, m, p, Some(&pa), Some(serde_json::json!({}))).await;
        assert_eq!(c, 403, "{m} {p}: {b}");
        if c == 403 {
            assert_eq!(b["code"], "ERR_FORBIDDEN");
        }
    }
    // ✅ GET /shared/refs 只读放行
    let (c, _) = req(&s.base, "GET", "/api/v1/shared/refs?project=p1", Some(&pa), None).await;
    assert_eq!(c, 200);

    // ❌ 项目面/账号/集群/全局
    for (m, p) in [
        ("POST", "/api/v1/projects"),
        ("DELETE", "/api/v1/projects/p1?force=true"),
        ("POST", "/api/v1/projects/p1/admins"),
        ("GET", "/api/v1/projects/p1/admins"),
        ("DELETE", "/api/v1/projects/p1/admins/alice"),
        ("POST", "/api/v1/admin/set-password"),
        ("POST", "/api/v1/admin/force-logout"),
        ("POST", "/api/v1/admin/snapshot"),
        ("POST", "/api/v1/admin/rotate-master-key"),
        ("GET", "/api/v1/cluster/members"),
        ("GET", "/api/v1/admin/retention-status"),
    ] {
        let (c, b) = req(&s.base, m, p, Some(&pa), Some(serde_json::json!({}))).await;
        assert_eq!(c, 403, "{m} {p}: {b}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pa_path_traversal_blocked() {
    let s = start().await;
    let admin = admin_login(&s.base).await;
    setup_p1_structure(&s.base, &admin).await;
    let (_, body) = pa_login(&s.base, "alice", "alicepw").await;
    let pa = body["token"].as_str().unwrap().to_string();

    // URL 编码/大写/尾斜杠/编码斜杠 → 全部 403（不落入项目路径匹配）
    for p in [
        "/api/v1/projects/%70%31/branches/dev/config",  // %70%31 = "p1" 编码
        "/api/v1/projects/P1/branches/dev/config",      // 大写（不匹配 valid_name → 非项目路径）
        "/api/v1/projects/p1/branches/dev/config/",     // 尾斜杠
        "/api/v1/projects/p1%2Fbranches/dev/config",    // 编码斜杠
        "/api/v1/projects/..%2Fp2/branches/dev/config", // 穿越
    ] {
        let (c, b) = req(&s.base, "GET", p, Some(&pa), None).await;
        assert!(c == 403 || c == 404, "path {p} 须被拒绝(403/404): {c} {b}");
        assert_ne!(c, 200, "path {p} 不得放行");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_negative_cases() {
    let s = start().await;
    let admin = admin_login(&s.base).await;
    let (_, body) = pa_login(&s.base, "alice", "alicepw").await;
    let pa_token = body["token"].as_str().unwrap().to_string();
    let secret = pa_token.trim_start_matches("pa.alice.").to_string();
    let _ = admin;

    // 伪造前缀：PA secret 拼 adm. → 路由到 sess/admin，hash 必败 → 401
    for forged in [
        format!("adm.{secret}"),
        "pa..x".to_string(),
        "pa.admin.x".to_string(),
        "pa.nobody.x".to_string(),
        pa_token[..pa_token.len().saturating_sub(4)].to_string(), // 截断
    ] {
        let (c, b) = req(&s.base, "GET", "/api/v1/projects/p1", Some(&forged), None).await;
        assert_eq!(c, 401, "forged {forged}: {b}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reveal_b2_regression() {
    let s = start().await;
    let admin = admin_login(&s.base).await;
    setup_p1_structure(&s.base, &admin).await;
    // p2 也建结构并发布一个 secret
    let (c, b) = req(&s.base, "PUT", "/api/v1/projects/p2/structure-draft", Some(&admin), Some(serde_json::json!({
        "base_version": 1,
        "groups": [{"name": "g", "items": [{"key": "sec", "type": "secret", "required": true, "secret": true}]}]
    }))).await;
    assert_eq!(c, 200, "p2 structure-draft: {b}");
    let (c, b) = req(&s.base, "POST", "/api/v1/projects/p2/structure-draft/publish", Some(&admin), Some(serde_json::json!({"comment": "c"}))).await;
    assert_eq!(c, 200, "p2 structure publish: {b}");
    let (c, b) = req(&s.base, "PUT", "/api/v1/projects/p2/branches/dev/draft", Some(&admin), Some(serde_json::json!({
        "updates": [{"group": "g", "key": "sec", "value": {"type": "secret", "ciphertext": {"enc": "aes-256-gcm", "v": 1, "dek_v": 1, "nonce": "AAAAAAAAAAAAAAAA", "ct": "AAAAAAAA", "edek": "AAAAAAAA", "edek_nonce": "AAAAAAAAAAAAAAAA"}}}], "deletes": []
    }))).await;
    assert_eq!(c, 200, "p2 draft: {b}");
    let (c, b) = req(&s.base, "POST", "/api/v1/projects/p2/branches/dev/publish", Some(&admin), Some(serde_json::json!({"comment": "c"}))).await;
    assert_eq!(c, 200, "p2 publish: {b}");

    let (_, body) = pa_login(&s.base, "alice", "alicepw").await;
    let pa = body["token"].as_str().unwrap().to_string();

    // PA reveal 自己项目 → 200（无 secret 则正常输出）
    let (c, _) = req(&s.base, "GET", "/v1/projects/p1/branches/dev/config?reveal=true", Some(&pa), None).await;
    assert_eq!(c, 200);
    // R1 回归：reveal 审计 operator 应为 pa:alice（不是 admin）
    let (c, b) = req(&s.base, "GET", "/api/v1/audit?limit=10", Some(&pa), None).await;
    assert_eq!(c, 200);
    assert!(
        b.as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "config_reveal" && e["operator"] == "pa:alice"),
        "reveal 审计 operator 须为 pa:alice: {b}"
    );
    // PA reveal 其他项目 → 403（B2 修复前是 200 越权）
    let (c, b) = req(&s.base, "GET", "/v1/projects/p2/branches/dev/config?reveal=true", Some(&pa), None).await;
    assert_eq!(c, 403, "B2 regression: {b}");
    // admin 全通
    let (c, _) = req(&s.base, "GET", "/v1/projects/p2/branches/dev/config?reveal=true", Some(&admin), None).await;
    assert_eq!(c, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_lifecycle_and_filters() {
    let s = start().await;
    let admin = admin_login(&s.base).await;
    setup_p1_structure(&s.base, &admin).await;
    let (_, body) = pa_login(&s.base, "alice", "alicepw").await;
    let pa = body["token"].as_str().unwrap().to_string();

    // GET /projects 过滤：PA 只见 p1
    let (c, b) = req(&s.base, "GET", "/api/v1/projects", Some(&pa), None).await;
    assert_eq!(c, 200);
    let arr = b.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "p1");

    // PA 发布产生审计（operator=pa:alice，project=p1）
    req(&s.base, "PUT", "/api/v1/projects/p1/branches/dev/draft", Some(&pa), Some(serde_json::json!({
        "updates": [{"group": "g", "key": "k", "value": {"type": "string", "str_value": "v"}}], "deletes": []
    }))).await;
    req(&s.base, "POST", "/api/v1/projects/p1/branches/dev/publish", Some(&pa), Some(serde_json::json!({"comment": "c"}))).await;
    let (c, b) = req(&s.base, "GET", "/api/v1/audit?limit=50", Some(&pa), None).await;
    assert_eq!(c, 200);
    let entries = b.as_array().unwrap();
    assert!(!entries.is_empty());
    // 全部条目 project=p1（强制过滤，全局条目不可见）
    for e in entries {
        assert_eq!(e["project"], "p1", "audit filter: {e}");
    }
    // 有 pa:alice operator 的发布条目（operator 贯穿验证）
    assert!(
        entries.iter().any(|e| e["operator"] == "pa:alice"),
        "operator=pa:alice 应出现在审计: {b}"
    );

    // force-logout 带 username（N16）：admin 踢 PA
    let (c, _) = req(&s.base, "POST", "/api/v1/admin/force-logout", Some(&admin), Some(serde_json::json!({"username": "alice"}))).await;
    assert_eq!(c, 200);
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1", Some(&pa), None).await;
    assert_eq!(c, 401, "被踢后旧 token 失效");

    // 重登 → 改密 → 旧 token 失效
    let (_, body) = pa_login(&s.base, "alice", "alicepw").await;
    let pa2 = body["token"].as_str().unwrap().to_string();
    let (c, _) = req(&s.base, "PUT", "/api/v1/projects/p1/admins/alice", Some(&admin), Some(serde_json::json!({"password": "new-pw"}))).await;
    assert_eq!(c, 204);
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1", Some(&pa2), None).await;
    assert_eq!(c, 401, "改密后旧 token 失效");
    // 新密码可登
    let (c, _) = pa_login(&s.base, "alice", "new-pw").await;
    assert_eq!(c, 200);

    // 删号 → 登录 401（账号不存在与错误密码同响应）
    req(&s.base, "DELETE", "/api/v1/projects/p1/admins/alice", Some(&admin), None).await;
    let (c, _) = pa_login(&s.base, "alice", "new-pw").await;
    assert_eq!(c, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_account_management_endpoints() {
    let s = start().await;
    let admin = admin_login(&s.base).await;

    // 创建 → 列表 → 重复创建 409 ERR_ACCOUNT_EXISTS → 修改 → 删除 → 404 ERR_ACCOUNT_NOT_FOUND
    let (c, b) = req(&s.base, "POST", "/api/v1/projects/p1/admins", Some(&admin), Some(serde_json::json!({"username": "bob", "password": "pw-bob"}))).await;
    assert_eq!(c, 201, "{b}");
    let (c, b) = req(&s.base, "GET", "/api/v1/projects/p1/admins", Some(&admin), None).await;
    assert_eq!(c, 200);
    assert_eq!(b.as_array().unwrap().len(), 2); // alice + bob（alice 来自播种）
    let (c, b) = req(&s.base, "POST", "/api/v1/projects/p1/admins", Some(&admin), Some(serde_json::json!({"username": "bob", "password": "x"}))).await;
    assert_eq!(c, 409);
    assert_eq!(b["code"], "ERR_ACCOUNT_EXISTS");
    // 不存在的项目创建账号 → 404
    let (c, _) = req(&s.base, "POST", "/api/v1/projects/ghost/admins", Some(&admin), Some(serde_json::json!({"username": "b2", "password": "x"}))).await;
    assert_eq!(c, 404);
    // 删除
    let (c, _) = req(&s.base, "DELETE", "/api/v1/projects/p1/admins/bob", Some(&admin), None).await;
    assert_eq!(c, 204);
    let (c, b) = req(&s.base, "DELETE", "/api/v1/projects/p1/admins/bob", Some(&admin), None).await;
    assert_eq!(c, 404);
    assert_eq!(b["code"], "ERR_ACCOUNT_NOT_FOUND");
    // 禁用名 admin
    let (c, _) = req(&s.base, "POST", "/api/v1/projects/p1/admins", Some(&admin), Some(serde_json::json!({"username": "admin", "password": "x"}))).await;
    assert!(c == 400 || c == 422, "禁用名 admin 应被拒绝: {c}");
}


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_session_relogin_and_heartbeat_ttl() {
    let s = start().await;
    let admin = admin_login(&s.base).await;
    let _ = admin;

    // 登录 → 登出 → 重登（N13 序列的登出半路径）
    let (_, b1) = pa_login(&s.base, "alice", "alicepw").await;
    let t1 = b1["token"].as_str().unwrap().to_string();
    let (c, _) = req(&s.base, "POST", "/api/v1/logout", Some(&t1), None).await;
    assert_eq!(c, 204);
    let (c, b2) = pa_login(&s.base, "alice", "alicepw").await;
    assert_eq!(c, 200, "登出后可重登");
    let t2 = b2["token"].as_str().unwrap().to_string();

    // heartbeat 返回续期后的 expires_at（B7：非 no-op）
    let (c, _) = req(&s.base, "POST", "/api/v1/heartbeat", None, None).await; // 无 token → 401
    assert_eq!(c, 401);
    let (c, _) = req(&s.base, "POST", "/api/v1/heartbeat", Some(&t1), None).await; // 旧 token 已登出 → 401
    assert_eq!(c, 401);
    let (c, hb) = req(&s.base, "POST", "/api/v1/heartbeat", Some(&t2), None).await; // 新 token 续期 → 200
    assert_eq!(c, 200, "heartbeat: {hb}");
    assert!(hb["expires_at"].is_i64(), "heartbeat 应返回续期时间: {hb}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_login_single_session() {
    let s = start().await;
    // 并发两路登录同一账号（N13：最坏一路 409，恰一会话成立）
    let (r1, r2) = tokio::join!(
        pa_login(&s.base, "alice", "alicepw"),
        pa_login(&s.base, "alice", "alicepw"),
    );
    let codes = [r1.0, r2.0];
    let ok = codes.iter().filter(|&&c| c == 200).count();
    let conflict = codes.iter().filter(|&&c| c == 409).count();
    assert_eq!(ok, 1, "并发登录恰一成功: {codes:?}");
    assert_eq!(conflict, 1, "另一路 409: {codes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_session_auto_relogin() {
    // S6/N13 核心分支：已有会话但已过期 → API 层自动登出+重登（不是 409）
    let s = start().await;
    // 直接向状态机注入一条已过期的 PA 会话（apply 不读墙钟，过期判定在 API 层）
    {
        let mut sm = s._state.sm.lock().unwrap();
        sm.apply(
            &Command::PaSessionLogin {
                username: "alice".into(),
                token_hash: dsh_core::token_hash("stale"),
                issued_at: 1,
                expires_at: Some(2), // 早已过期（now_ms() >> 2）
                device_id: "cli".into(),
            },
            1,
        )
        .unwrap();
    }
    // HTTP 重登：应走「复查已过期 → 自动登出 → 重登成功」路径
    let (c, b) = pa_login(&s.base, "alice", "alicepw").await;
    assert_eq!(c, 200, "过期会话应自动重登而非 409: {b}");
    let token = b["token"].as_str().unwrap();
    // 新 token 可用
    let (c, _) = req(&s.base, "GET", "/api/v1/projects/p1", Some(token), None).await;
    assert_eq!(c, 200);
}
