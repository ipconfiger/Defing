//! dsh 二进制（组装器）：解析 CLI → 装配存储/Raft/状态 → dsh-api 路由 → 监听。
//! HTTP handler 见 dsh-api；发布编排见 dsh-publish；可观测见 dsh-observability；
//! watch 见 dsh-watch；状态机见 dsh-core。本文件仅负责启动装配。

use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use dsh_api::ApiState;
use dsh_core::{InMemoryStore, StateMachine};
use dsh_crypto::{load_master_key, Cipher};
use dsh_raft::{
    HttpNetworkFactory, LogStore, NodeInfo as RaftNodeInfo, RaftServerState, StateMachineStore,
};
use dsh_storage::RedbStorage;
use dsh_watch::WatchHub;

// ---------------- 配置 ----------------

/// `dsh admin <子命令>`：管理员运维客户端（design-v2 §13.2 / design-v3 §6）。
#[derive(Subcommand, Debug)]
enum AdminCmd {
    /// 生成新主密钥（base64 32B）并打印指引
    GenMasterKey,
    /// 轮换主密钥（调管理面 API；DEK 重包由后台任务执行）
    RotateMasterKey {
        /// base64 32B 新 KEK
        new_key: String,
    },
    /// 强制下线当前管理员会话（I7 兜底）
    ForceLogout,
    /// 修改管理员密码（旧会话失效）
    SetPassword {
        /// 新密码（≥6 位）
        password: String,
    },
    /// learner → voter
    Promote {
        #[arg(long)]
        node: u64,
    },
    /// 移除节点
    RemoveNode {
        #[arg(long)]
        node: u64,
    },
    /// 触发备份快照（状态机 KV dump；可 --out 存盘）
    Snapshot {
        #[arg(long)]
        out: Option<String>,
    },
    /// 查看保留策略状态（--version-retention / --audit-retention 配置）
    RetentionStatus,
}

/// 顶层子命令命名空间（当前仅 admin 运维）。
#[derive(Subcommand, Debug)]
enum Command {
    /// 管理员运维（design-v2 §13.2）：gen-master-key / rotate-master-key /
    /// force-logout / set-password / promote / remove-node / snapshot / retention-status
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
}

#[derive(Parser, Debug)]
#[command(name = "dsh", version, about = "Defing 分布式配置文档服务")]
struct Cli {
    /// 单节点联调模式（无 Raft，直接 apply 状态机）
    #[arg(long)]
    dev_single: bool,
    /// 集群模式：节点 ID
    #[arg(long)]
    node_id: Option<u64>,
    /// 集群模式：首节点自举
    #[arg(long)]
    bootstrap: bool,
    /// 集群模式：加入集群（指定任一实例的 HTTP 端点，如 http://127.0.0.1:8384）
    #[arg(long)]
    join: Option<String>,
    /// HTTP 监听地址（管理面）
    #[arg(long, default_value = "127.0.0.1:8384")]
    http_addr: String,
    /// Raft 内部 RPC 地址
    #[arg(long, default_value = "127.0.0.1:8385")]
    raft_addr: String,
    /// 数据面 gRPC 地址（A1：tonic 服务挂载于此）
    #[arg(long, default_value = "127.0.0.1:8383")]
    grpc_addr: String,
    /// 数据目录（集群模式必填；dev-single 缺省内存）
    #[arg(long)]
    data_dir: Option<String>,
    /// 主密钥文件（raw 32B；或 DSH_MASTER_KEY 环境变量 base64）
    #[arg(long)]
    master_key_file: Option<String>,
    /// 版本保留数（0=全量保留；后台裁剪任务仅在 >0 时启用）
    #[arg(long, default_value_t = 0)]
    version_retention: u64,
    /// 审计保留条数（0=不裁剪；默认 100k 条，design-v2）
    #[arg(long, default_value_t = 100000)]
    audit_retention: u64,
    /// 管理员密码（缺省首启随机生成并打印；admin 客户端模式用于登录）
    #[arg(long, global = true)]
    admin_password: Option<String>,
    /// 会话 TTL 秒数（0 = 不自动过期；默认 24h）
    #[arg(long, default_value_t = 86400)]
    session_ttl: u64,
    /// 数据面 gRPC 访问令牌（metadata authorization: Bearer <token>；缺省开放，仅建议集群启用）
    #[arg(long)]
    data_plane_token: Option<String>,
    /// 生成新主密钥（base64 32B）并退出
    #[arg(long)]
    gen_master_key: bool,
    /// 轮换主密钥（客户端模式）：向 --admin-endpoint 发起轮换后退出（需 --admin-password）
    #[arg(long)]
    rotate_master_key: Option<String>,
    /// 管理面端点（客户端模式：rotate-master-key / admin <cmd> 用）
    #[arg(long, global = true, default_value = "http://127.0.0.1:8384")]
    admin_endpoint: String,
    /// 管理面会话令牌（客户端模式；缺省时用 --admin-password 登录；单会话下建议直接传 token）
    #[arg(long, global = true)]
    admin_token: Option<String>,
    /// 顶层子命令（dsh admin <cmd>；客户端模式，不启动服务）
    #[command(subcommand)]
    cmd: Option<Command>,
}

// ---------------- 工具 ----------------

fn new_token() -> String {
    let b: [u8; 16] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 管理员客户端 token：优先 --admin-token（单会话下 login 会 409）；缺省登录获取。
async fn admin_token(cli: &Cli) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(t) = &cli.admin_token {
        return Ok(t.clone());
    }
    let pw = cli
        .admin_password
        .clone()
        .ok_or("需要 --admin-token 或 --admin-password")?;
    let client = reqwest::Client::new();
    let login: serde_json::Value = client
        .post(format!("{}/api/v1/login", cli.admin_endpoint))
        .json(&serde_json::json!({ "password": pw }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = login["token"]
        .as_str()
        .ok_or_else(|| "login failed (bad password?)".to_string())?;
    Ok(token.to_string())
}

/// `dsh admin <cmd>` 分派（客户端模式，调管理面 HTTP）。
async fn run_admin_cmd(
    cli: &Cli,
    cmd: &AdminCmd,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match cmd {
        AdminCmd::GenMasterKey => {
            println!("{}", dsh_crypto::Cipher::generate_master_key());
            return Ok(());
        }
        AdminCmd::RotateMasterKey { new_key } => {
            let token = admin_token(cli).await?;
            let resp = reqwest::Client::new()
                .post(format!(
                    "{}/api/v1/admin/rotate-master-key",
                    cli.admin_endpoint
                ))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "new_key": new_key }))
                .send()
                .await?;
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            println!("{body}");
            if !status.is_success() {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }
    let token = admin_token(cli).await?;
    let client = reqwest::Client::new();
    let base = cli.admin_endpoint.trim_end_matches('/');
    match cmd {
        AdminCmd::ForceLogout => {
            let resp = client
                .post(format!("{base}/api/v1/admin/force-logout"))
                .bearer_auth(&token)
                .send()
                .await?;
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            println!("{body}");
            if !status.is_success() {
                std::process::exit(1);
            }
        }
        AdminCmd::SetPassword { password } => {
            let resp = client
                .post(format!("{base}/api/v1/admin/set-password"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "password": password }))
                .send()
                .await?;
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            println!("{body}");
            if !status.is_success() {
                std::process::exit(1);
            }
            eprintln!("密码已修改，旧会话已下线，请用新密码重新登录");
        }
        AdminCmd::Promote { node } => {
            let resp = client
                .post(format!("{base}/api/v1/cluster/promote"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "node_id": node }))
                .send()
                .await?;
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            println!("{body}");
            if !status.is_success() {
                std::process::exit(1);
            }
        }
        AdminCmd::RemoveNode { node } => {
            let resp = client
                .post(format!("{base}/api/v1/cluster/remove"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "node_id": node }))
                .send()
                .await?;
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            println!("{body}");
            if !status.is_success() {
                std::process::exit(1);
            }
        }
        AdminCmd::Snapshot { out } => {
            let resp = client
                .get(format!("{base}/api/v1/admin/snapshot"))
                .bearer_auth(&token)
                .send()
                .await?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                eprintln!("snapshot failed: {text}");
                std::process::exit(1);
            }
            match out {
                Some(path) => {
                    std::fs::write(path, &text)?;
                    eprintln!("快照已写入 {path}");
                }
                None => println!("{text}"),
            }
        }
        AdminCmd::RetentionStatus => {
            let resp = client
                .get(format!("{base}/api/v1/admin/retention-status"))
                .bearer_auth(&token)
                .send()
                .await?;
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            println!("{body}");
            if !status.is_success() {
                std::process::exit(1);
            }
        }
        AdminCmd::GenMasterKey | AdminCmd::RotateMasterKey { .. } => unreachable!(),
    }
    Ok(())
}

/// 加入集群：向目标端点发起 join（需命中 leader；带重试与超时）。
async fn join_cluster(
    _raft: &dsh_raft::RaftHandle,
    node_id: u64,
    node: RaftNodeInfo,
    join_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let resp = client
            .post(format!("{join_url}/api/v1/cluster/join"))
            .json(&serde_json::json!({
                "node_id": node_id,
                "http_addr": node.http_addr,
                "raft_addr": node.raft_addr,
            }))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => return Ok(()),
            _ => {
                if tokio::time::Instant::now() >= deadline {
                    return Err("join timed out (no leader responded)".into());
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }
}

fn resolve_admin_password(cli: &Cli, node_label: &str) -> Arc<str> {
    match &cli.admin_password {
        Some(p) => Arc::from(p.as_str()),
        None => {
            let gen = format!("dsh-admin-{}", new_token());
            eprintln!("{node_label} 管理员密码 = {gen}（请使用 --admin-password 显式设置）");
            Arc::from(gen.as_str())
        }
    }
}

// ---------------- 主入口 ----------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let cli = Cli::parse();
    // `dsh admin <cmd>` 客户端模式（不启动服务；需 --admin-endpoint）
    if let Some(Command::Admin { cmd }) = &cli.cmd {
        return run_admin_cmd(&cli, cmd).await;
    }
    let hub = WatchHub::new();
    // 主密钥（secret 项加密/解密；I8）
    let master_key = load_master_key(
        std::env::var("DSH_MASTER_KEY").ok().as_deref(),
        cli.master_key_file.as_deref(),
    )
    .map_err(|e| format!("master key: {e}"))?;
    // 主密钥环：文件密钥 + 环文件历史 KEK（轮换后重启可解旧数据，CRY-002）
    let cipher = master_key.map(|k| {
        let ring_path = cli
            .master_key_file
            .as_deref()
            .map(dsh_crypto::ring_file_path);
        let ring_entries = ring_path
            .as_ref()
            .and_then(|p| dsh_crypto::load_ring(p).ok())
            .unwrap_or_default();
        let mut entries = vec![k];
        entries.extend(ring_entries);
        Arc::new(Cipher::with_keyring(dsh_crypto::KeyRing::from_entries(
            entries,
        )))
    });

    // ---- 客户端模式（不启动服务）----
    if cli.gen_master_key {
        println!("{}", dsh_crypto::Cipher::generate_master_key());
        return Ok(());
    }
    if let Some(new_key) = &cli.rotate_master_key {
        let client = reqwest::Client::new();
        // 单会话（I7）下已有会话时 login 会 409 → 优先用 --admin-token；缺省才登录
        let token = match &cli.admin_token {
            Some(t) => t.clone(),
            None => {
                let pw = cli
                    .admin_password
                    .clone()
                    .ok_or("--rotate-master-key 需要 --admin-password 或 --admin-token")?;
                let login: serde_json::Value = client
                    .post(format!("{}/api/v1/login", cli.admin_endpoint))
                    .json(&serde_json::json!({ "password": pw }))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                login["token"]
                    .as_str()
                    .ok_or("login failed (bad password?)")?
                    .to_string()
            }
        };
        let resp = client
            .post(format!(
                "{}/api/v1/admin/rotate-master-key",
                cli.admin_endpoint
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "new_key": new_key }))
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        println!("{body}");
        if !status.is_success() {
            std::process::exit(1);
        }
        return Ok(());
    }

    if cli.dev_single {
        let store: Box<dyn dsh_core::Store> = match &cli.data_dir {
            Some(dir) => Box::new(RedbStorage::open(dir)?),
            None => Box::new(InMemoryStore::new()),
        };
        let sm = StateMachine::new(store);
        let admin_password = resolve_admin_password(&cli, "首次启动");
        let app = ApiState::with_retention(
            Arc::new(Mutex::new(sm)),
            hub,
            None,
            None,
            cipher,
            std::time::Duration::from_secs(cli.session_ttl),
            admin_password,
            cli.master_key_file
                .as_deref()
                .map(dsh_crypto::ring_file_path),
            cli.version_retention,
            cli.audit_retention,
        );
        spawn_grpc(&cli, app.clone());
        let router = dsh_api::build_router(app);
        let listener = tokio::net::TcpListener::bind(&cli.http_addr).await?;
        eprintln!("dsh --dev-single listening on http://{}", cli.http_addr);
        axum::serve(listener, router).await?;
        return Ok(());
    }

    // ---------- 集群模式 ----------
    let data_dir = cli.data_dir.clone().ok_or("集群模式需要 --data-dir")?;
    let node_id = cli.node_id.ok_or("集群模式需要 --node-id")?;

    let storage = RedbStorage::open(&data_dir)?;
    let db = storage.raw_db();
    // 重启恢复：raft-meta 非空说明该节点已有持久化状态 → 无需 --bootstrap/--join，自动 resume
    let has_state = {
        use redb::ReadableDatabase;
        let txn = db
            .begin_read()
            .map_err(|e| format!("读取 raft-meta 失败: {e}"))?;
        match txn.open_table::<&[u8], &[u8]>(dsh_storage::TBL_RAFT_META) {
            Ok(tbl) => tbl
                .range::<&[u8]>(..)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false),
            Err(_) => false,
        }
    };
    if !cli.bootstrap && cli.join.is_none() && !has_state {
        return Err("集群模式需要 --bootstrap、--join 或已有数据目录".into());
    }
    let sm = Arc::new(Mutex::new(StateMachine::new(Box::new(storage))));
    let sm_store = Arc::new(StateMachineStore::new(sm.clone(), db.clone()));
    // 集群 watch：raft apply 事件 → hub（SSE）
    hub.spawn_raft_forward(sm_store.clone());
    let log_store = LogStore::new(db.clone());

    let node_info = RaftNodeInfo {
        grpc_addr: cli.grpc_addr.clone(),
        http_addr: cli.http_addr.clone(),
        raft_addr: cli.raft_addr.clone(),
    };
    let network = HttpNetworkFactory::new();
    let raft = dsh_raft::new_raft_node(
        node_id,
        node_info.clone(),
        log_store,
        sm_store,
        &network,
        dsh_raft::dev_config(),
    )
    .await?;

    if cli.bootstrap {
        dsh_raft::initialize_single(&raft, node_id, node_info.clone()).await?;
        eprintln!("node {node_id} bootstrap done");
    } else if let Some(join_url) = &cli.join {
        join_cluster(&raft, node_id, node_info.clone(), join_url).await?;
        eprintln!("node {node_id} join requested -> {join_url}");
    } else if has_state {
        eprintln!("node {node_id} resuming from persisted state (auto-rejoin)");
    }

    // 后台任务（仅 leader 执行）：版本裁剪 + 审计保留
    {
        // 由 raft.metrics() watch 推导 is_leader
        let (leader_tx, leader_rx) = tokio::sync::watch::channel(false);
        let metrics = raft.metrics();
        tokio::spawn(async move {
            let mut rx = metrics;
            while rx.changed().await.is_ok() {
                let m = rx.borrow().clone();
                let _ = leader_tx.send(m.current_leader == Some(node_id));
            }
        });
        let mut scheduler = dsh_jobs::JobScheduler::new();
        if cli.version_retention > 0 {
            scheduler.add(dsh_jobs::VersionRetention {
                keep: cli.version_retention as usize,
            });
        }
        if cli.audit_retention > 0 {
            scheduler.add(dsh_jobs::AuditRetention {
                keep: cli.audit_retention as usize,
            });
        }
        scheduler.spawn(sm.clone(), leader_rx);
    }

    // Raft RPC 服务（raft_addr）
    let raft_state = RaftServerState { raft: raft.clone() };
    let raft_router = dsh_raft::raft_router(raft_state);
    let raft_addr = cli.raft_addr.clone();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&raft_addr)
            .await
            .expect("bind raft addr");
        axum::serve(listener, raft_router)
            .await
            .expect("raft server");
    });

    let admin_password = resolve_admin_password(&cli, &format!("节点 {node_id}"));
    let app = ApiState::with_retention(
        sm.clone(),
        hub,
        Some(raft.clone()),
        Some(node_id),
        cipher,
        std::time::Duration::from_secs(cli.session_ttl),
        admin_password,
        cli.master_key_file
            .as_deref()
            .map(dsh_crypto::ring_file_path),
        cli.version_retention,
        cli.audit_retention,
    );
    spawn_grpc(&cli, app.clone());
    let router = dsh_api::build_router(app);
    let listener = tokio::net::TcpListener::bind(&cli.http_addr).await?;
    eprintln!(
        "dsh node {node_id} listening on http://{} (raft {})",
        cli.http_addr, cli.raft_addr
    );
    axum::serve(listener, router).await?;
    Ok(())
}

/// 数据面 gRPC 服务（模块 05）：ConfigService 挂载于 --grpc-addr（默认 :8383）。
fn spawn_grpc(cli: &Cli, state: ApiState) {
    let svc = dsh_api::grpc::config_service_server::ConfigServiceServer::with_interceptor(
        dsh_api::grpc::ConfigGrpcService { state },
        dsh_api::grpc::data_plane_interceptor(cli.data_plane_token.clone()),
    );
    let addr: std::net::SocketAddr = match cli.grpc_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("invalid --grpc-addr {}: {e}", cli.grpc_addr);
            return;
        }
    };
    let grpc_addr = cli.grpc_addr.clone();
    tokio::spawn(async move {
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(svc)
            .serve(addr)
            .await
        {
            eprintln!("grpc server on {grpc_addr} failed: {e}");
        } else {
            eprintln!("dsh gRPC data plane listening on {grpc_addr}");
        }
    });
}
