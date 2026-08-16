//! gRPC 数据面集成测试（A1）：GetConfig / GetItem / Watch / ListMembers + 鉴权拦截器。
//! 使用真实 TCP 监听 + 生成的客户端。

use std::sync::{Arc, RwLock};

use dsh_api::grpc::{
    config_service_client::ConfigServiceClient, config_service_server::ConfigServiceServer,
    ConfigGrpcService, GetConfigRequest, GetItemRequest, ListMembersRequest, WatchRequest,
};
use dsh_api::ApiState;
use dsh_core::command::Command;
use dsh_core::model::{BranchName, ProjectId, Value};
use dsh_core::InMemoryStore;
use dsh_core::StateMachine;
use dsh_crypto::Cipher;
use dsh_testkit::seed_demo_project;
use dsh_watch::WatchHub;

fn seed_sm(sm: &RwLock<StateMachine>) {
    // testkit 播种：项目 + 结构(host/port/pass secret) + dev 草稿(host/port) + 发布(v2)
    seed_demo_project(sm, "p").unwrap();
    // 追加 secret 项值（明文不落库，测试直接写密文）
    let mut g = sm.write().unwrap();
    g.apply(
        &Command::DraftUpdate {
            project: "p".into(),
            branch: BranchName("dev".into()),
            updates: vec![
                dsh_core::command::DraftUpdateItem {
                    group: "redis".into(),
                    key: "host".into(),
                    value: Value::String("10.0.0.1".into()),
                },
                dsh_core::command::DraftUpdateItem {
                    group: "redis".into(),
                    key: "pass".into(),
                    value: Value::Secret(Cipher::new([9u8; 32]).encrypt_secret(b"s3cret").unwrap()),
                },
            ],
            deletes: vec![],

            operator: String::new(),
            ts: 0,
            expected_draft_rev: None,
        },
        6,
    )
    .unwrap();
    g.apply(
        &Command::Publish {
            project: "p".into(),
            branch: BranchName("dev".into()),
            comment: "v3".into(),
            request_id: "r2".into(),

            operator: String::new(),
            ts: 0,
        },
        7,
    )
    .unwrap();
}

async fn start_server(token: Option<String>) -> (String, ApiState) {
    let sm = Arc::new(RwLock::new(StateMachine::new(Box::new(
        InMemoryStore::new(),
    ))));
    seed_sm(&sm);
    let hub = WatchHub::new();
    let state = ApiState::new(
        sm,
        hub,
        None,
        None,
        None,
        std::time::Duration::from_secs(86400),
        "pw".into(),
        None,
    );
    let svc = ConfigServiceServer::with_interceptor(
        ConfigGrpcService {
            state: state.clone(),
        },
        dsh_api::grpc::data_plane_interceptor(token),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (format!("http://{addr}"), state)
}

async fn client_at(url: &str) -> ConfigServiceClient<tonic::transport::Channel> {
    ConfigServiceClient::connect(url.to_string()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_config_and_get_item() {
    let (url, _state) = start_server(None).await;
    let mut client = client_at(&url).await;

    let snap = client
        .get_config(GetConfigRequest {
            project: "p".into(),
            branch: "dev".into(),
            version: 0,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(snap.version, 3); // testkit v2 + secret v3
    assert_eq!(snap.structure_version, 2); // 结构发布后版本=2（base_version=1 → published 2）
    let host = snap.groups.get("redis").unwrap().items.get("host").unwrap();
    assert!(!host.masked);
    assert_eq!(host.r#type, 1); // STRING

    // secret：脱敏 + masked 标记（数据面不解密）
    let pass = snap.groups.get("redis").unwrap().items.get("pass").unwrap();
    assert!(pass.masked);
    let masked_val = match &pass.data {
        Some(dsh_api::grpc::value::Data::StrValue(s)) => s.as_str(),
        _ => "",
    };
    assert!(!masked_val.contains("s3cret"));

    // GetItem 单值
    let item = client
        .get_item(GetItemRequest {
            project: "p".into(),
            branch: "dev".into(),
            group: "redis".into(),
            key: "host".into(),
            version: 0,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let got = item.value.unwrap();
    assert!(!got.masked);
    match got.data.unwrap() {
        dsh_api::grpc::value::Data::StrValue(s) => assert_eq!(s, "10.0.0.1"),
        other => panic!("expected str value, got {other:?}"),
    }

    // 不存在的 item → NotFound
    let err = client
        .get_item(GetItemRequest {
            project: "p".into(),
            branch: "dev".into(),
            group: "redis".into(),
            key: "nope".into(),
            version: 0,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_delivers_publish_events() {
    let (url, state) = start_server(None).await;
    let mut client = client_at(&url).await;

    // 订阅（after_version=2 = 当前活动版本）→ 只收后续事件
    let mut stream = client
        .watch(WatchRequest {
            project: "p".into(),
            branch: "dev".into(),
            after_version: 2,
        })
        .await
        .unwrap()
        .into_inner();

    // 经 ApiState 发布 v3（写路径带 hub 广播）：先写草稿再发布
    state
        .publish
        .update_draft(
            &ProjectId("p".into()),
            &BranchName("dev".into()),
            vec![dsh_core::command::DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("10.0.0.2".into()),
            }],
            vec![],
            None,
            "test",
        )
        .await
        .unwrap();
    state
        .publish
        .publish(
            &ProjectId("p".into()),
            &BranchName("dev".into()),
            "v3",
            "r3",
            "test",
        )
        .await
        .unwrap();

    let ev = tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
        .await
        .expect("watch timeout")
        .unwrap()
        .expect("stream message");
    assert_eq!(ev.version, 3);
    assert!(!ev.changes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_interceptor_enforces_token() {
    let (url, _state) = start_server(Some("tok-123".into())).await;

    // 无 token → Unauthenticated
    let mut plain = client_at(&url).await;
    let err = plain
        .get_config(GetConfigRequest {
            project: "p".into(),
            branch: "dev".into(),
            version: 0,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 带正确 token → 成功
    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut authed =
        ConfigServiceClient::with_interceptor(channel, |mut req: tonic::Request<()>| {
            req.metadata_mut()
                .insert("authorization", "Bearer tok-123".parse().unwrap());
            Ok(req)
        });
    let snap = authed
        .get_config(GetConfigRequest {
            project: "p".into(),
            branch: "dev".into(),
            version: 0,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(snap.version, 3); // testkit v2 + secret v3
    let _ = &mut plain;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_members_dev_single_fails_precondition() {
    let (url, _state) = start_server(None).await;
    let mut client = client_at(&url).await;
    let err = client
        .list_members(ListMembersRequest {})
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

// ==================== G3 灰度数据面（design/g3-dataplane.md，D26/D27/D25） ====================

/// G3：gRPC get_config / get_item 按身份 resolve——命中读灰度快照、未命中/无身份读稳定（D26/D27/Q6）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gray_data_plane_resolves_by_identity() {
    let (url, state) = start_server(None).await;
    let mut client = client_at(&url).await;

    // 直接写状态机：新草稿（host=gray-host）→ GrayPublish（规则 zone=cn-north-1）
    {
        let mut sm = state.sm.write().unwrap();
        sm.apply(
            &Command::DraftUpdate {
                project: "p".into(),
                branch: BranchName("dev".into()),
                updates: vec![dsh_core::command::DraftUpdateItem {
                    group: "redis".into(),
                    key: "host".into(),
                    value: Value::String("gray-host".into()),
                }],
                deletes: vec![],
                operator: String::new(),
                ts: 0,
                expected_draft_rev: None,
            },
            100,
        )
        .unwrap();
        sm.apply(
            &Command::GrayPublish {
                project: "p".into(),
                branch: BranchName("dev".into()),
                rule: dsh_core::model::GrayRule {
                    match_labels: vec![dsh_core::model::LabelSelector {
                        key: "zone".into(),
                        value: "cn-north-1".into(),
                    }],
                    ip_cidrs: vec![],
                    percentage: None,
                },
                comment: "g".into(),
                request_id: "g1".into(),
                operator: String::new(),
                ts: 0,
            },
            101,
        )
        .unwrap();
    }
    let north: std::collections::HashMap<String, String> =
        [("zone".to_string(), "cn-north-1".to_string())].into();
    let south: std::collections::HashMap<String, String> =
        [("zone".to_string(), "cn-south-1".to_string())].into();

    // ① 命中（instance_id + labels）→ 灰度内容 + gray=true + resolved_version=gray_seq
    let snap = client
        .get_config(GetConfigRequest {
            project: "p".into(),
            branch: "dev".into(),
            version: 0,
            instance_id: "web-1".into(),
            labels: north.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(snap.gray, "身份命中 → gray=true");
    assert_eq!(snap.resolved_version, 1, "resolved_version = gray_seq");
    let host = snap.groups.get("redis").unwrap().items.get("host").unwrap();
    match &host.data {
        Some(dsh_api::grpc::value::Data::StrValue(s)) => assert_eq!(s, "gray-host"),
        other => panic!("expected str, got {other:?}"),
    }

    // ② 未命中 → 稳定版 + gray=false（active=3：testkit v2 + secret v3）
    let snap2 = client
        .get_config(GetConfigRequest {
            project: "p".into(),
            branch: "dev".into(),
            version: 0,
            instance_id: "web-2".into(),
            labels: south.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!snap2.gray);
    assert_eq!(snap2.resolved_version, 3);
    let host2 = snap2
        .groups
        .get("redis")
        .unwrap()
        .items
        .get("host")
        .unwrap();
    match &host2.data {
        Some(dsh_api::grpc::value::Data::StrValue(s)) => assert_eq!(s, "10.0.0.1"),
        other => panic!("expected str, got {other:?}"),
    }

    // ③ 无身份（旧客户端）→ 稳定版（Q2 向后兼容）
    let snap3 = client
        .get_config(GetConfigRequest {
            project: "p".into(),
            branch: "dev".into(),
            version: 0,
            instance_id: String::new(),
            labels: std::collections::HashMap::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!snap3.gray, "无身份永不进灰度（Q2）");

    // ④ get_item 同分流（Q6）
    let item = client
        .get_item(GetItemRequest {
            project: "p".into(),
            branch: "dev".into(),
            group: "redis".into(),
            key: "host".into(),
            version: 0,
            instance_id: "web-1".into(),
            labels: north,
        })
        .await
        .unwrap()
        .into_inner();
    match &item.value.unwrap().data {
        Some(dsh_api::grpc::value::Data::StrValue(s)) => {
            assert_eq!(s, "gray-host", "get_item 必须同样 resolve")
        }
        other => panic!("expected str, got {other:?}"),
    }
}

/// G3/D25：gRPC watch 灰度事件永不按版本过滤——gray:true 且 version ≤ last（active 未变）
/// 的 GrayPublish 事件仍投递（Q4：promote/abort 补发不丢）；last 游标不因 gray 事件倒挂。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gray_watch_delivers_gray_events() {
    let (url, state) = start_server(None).await;
    let mut client = client_at(&url).await;

    // 订阅：after_version=3（当前 active）→ last=3
    let mut stream = client
        .watch(WatchRequest {
            project: "p".into(),
            branch: "dev".into(),
            after_version: 3,
        })
        .await
        .unwrap()
        .into_inner();

    // 灰度发布（sm.apply + hub 手动广播，模拟写路径）——事件 gray=true、version=3（active 未变 ≤ last）
    let events = {
        let mut sm = state.sm.write().unwrap();
        sm.apply(
            &Command::DraftUpdate {
                project: "p".into(),
                branch: BranchName("dev".into()),
                updates: vec![dsh_core::command::DraftUpdateItem {
                    group: "redis".into(),
                    key: "host".into(),
                    value: Value::String("gray-host".into()),
                }],
                deletes: vec![],
                operator: String::new(),
                ts: 0,
                expected_draft_rev: None,
            },
            100,
        )
        .unwrap();
        sm.apply(
            &Command::GrayPublish {
                project: "p".into(),
                branch: BranchName("dev".into()),
                rule: dsh_core::model::GrayRule {
                    match_labels: vec![dsh_core::model::LabelSelector {
                        key: "zone".into(),
                        value: "cn-north-1".into(),
                    }],
                    ip_cidrs: vec![],
                    percentage: None,
                },
                comment: "g".into(),
                request_id: "g1".into(),
                operator: String::new(),
                ts: 0,
            },
            101,
        )
        .unwrap()
    };
    for e in &events {
        state.hub.publish(e);
    }

    // GrayPublish 事件必须投递（尽管 version=3 == last）
    let ev = tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
        .await
        .expect("watch timeout")
        .unwrap()
        .expect("stream message");
    assert!(ev.gray, "灰度事件 gray=true");
    assert_eq!(
        ev.version, 3,
        "GrayPublish 事件 version=active（未变）仍投递（D25）"
    );

    // 普通发布 v4 → 版本推进，正常投递（验证游标未因 gray 事件倒挂）
    state
        .publish
        .update_draft(
            &ProjectId("p".into()),
            &BranchName("dev".into()),
            vec![dsh_core::command::DraftUpdateItem {
                group: "redis".into(),
                key: "host".into(),
                value: Value::String("10.0.0.3".into()),
            }],
            vec![],
            None,
            "test",
        )
        .await
        .unwrap();
    state
        .publish
        .publish(
            &ProjectId("p".into()),
            &BranchName("dev".into()),
            "v4",
            "r4",
            "test",
        )
        .await
        .unwrap();
    let ev = tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
        .await
        .expect("watch timeout")
        .unwrap()
        .expect("stream message");
    assert!(!ev.gray);
    assert_eq!(
        ev.version, 4,
        "last 游标未因 gray 事件倒挂（普通事件正常推进）"
    );
}
