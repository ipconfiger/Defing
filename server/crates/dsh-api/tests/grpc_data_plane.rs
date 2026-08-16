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
