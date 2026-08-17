//! dsh 二进制（组装器）：解析 CLI → 装配存储/Raft/状态 → dsh-api 路由 → 监听。
//! HTTP handler 见 dsh-api；发布编排见 dsh-publish；可观测见 dsh-observability；
//! watch 见 dsh-watch；状态机见 dsh-core。本文件仅负责启动装配。

use std::sync::{Arc, RwLock};

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
    /// 灰度自动回滚阈值（本地 HTTP 5xx 比例 %；0=禁用，G5/D33）
    #[arg(long, default_value_t = 0.0)]
    gray_rollback_threshold: f64,
    /// 灰度自动回滚检查间隔秒（测试可调小；默认 60）
    #[arg(long, default_value_t = 60)]
    gray_rollback_interval: u64,
    /// 发布校验策略（G1/D35）：block=校验失败拒绝（默认）| warn=仅记录继续发布
    #[arg(long, value_enum, default_value_t = PolicyArg::Block)]
    publish_policy: PolicyArg,
    /// 共享发布级联模式（G1/D36）：auto=自动级联引用分支（默认）| manual=只更共享版本
    #[arg(long, value_enum, default_value_t = CascadeArg::Auto)]
    shared_cascade: CascadeArg,
    /// 读取模式（G1/D37）：linear=ReadIndex 门控读已提交（默认）| stale=本地直读
    #[arg(long, value_enum, default_value_t = ReadArg::Linear)]
    read_mode: ReadArg,
    /// 管理员密码（缺省首启随机生成并打印；admin 客户端模式用于登录）
    #[arg(long, global = true)]
    admin_password: Option<String>,
    /// 会话 TTL 秒数（0 = 不自动过期；默认 24h）
    #[arg(long, default_value_t = 86400)]
    session_ttl: u64,
    /// 数据面 gRPC 访问令牌（metadata authorization: Bearer <token>；缺省开放，仅建议集群启用）
    #[arg(long)]
    data_plane_token: Option<String>,
    /// 集群 join 引导令牌（/api/v1/cluster/join 需 Bearer 匹配；缺省不校验）
    #[arg(long)]
    join_token: Option<String>,
    /// Raft 内部 RPC 共享令牌（缺省不校验；启用后集群内所有节点必须传相同值）
    #[arg(long)]
    raft_token: Option<String>,
    /// 可信代理 CIDR 列表（逗号分隔，如 "10.0.0.0/8,192.168.0.0/16"）：
    /// 仅信任来自这些网段的 X-Forwarded-For 作为登录节流键；未配置时忽略 XFF 用对端地址（F4）
    #[arg(long)]
    trusted_proxy: Option<String>,
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
/// join_token 为 Some 时请求携带 `Authorization: Bearer <token>`（与节点 --join-token 匹配）。
async fn join_cluster(
    _raft: &dsh_raft::RaftHandle,
    node_id: u64,
    node: RaftNodeInfo,
    join_url: &str,
    join_token: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let mut req = client.post(format!("{join_url}/api/v1/cluster/join"));
        if let Some(token) = join_token {
            req = req.bearer_auth(token);
        }
        let resp = req
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

/// 集群主密钥轮换钩子类型（F7b：入参为 RotateMasterKey 命令，解密由实现方负责）。
type RotationHook = Arc<dyn Fn(&dsh_core::command::Command) + Send + Sync>;

/// 集群轮换钩子：Raft apply 到 RotateMasterKey 时更新本节点 keyring 并持久化 ring 文件
/// （幂等：已含该 KEK 则跳过，重放安全；持久化失败不切换内存，保持可解）。
/// F7b：新命令载荷为 kek_enc（当前 KEK 自加密）——逐个尝试 keyring 内 KEK 解开；
/// 旧日志为 kek 明文，直接使用。
fn cluster_rotation_hook(
    key_file: Option<&str>,
    cipher: Option<Arc<Cipher>>,
) -> Option<RotationHook> {
    let cipher = cipher?;
    let ring_path = key_file.map(dsh_crypto::ring_file_path);
    Some(Arc::new(move |cmd: &dsh_core::command::Command| {
        let dsh_core::command::Command::RotateMasterKey { kek, kek_enc } = cmd else {
            return;
        };
        // 解析明文 KEK（32B）
        let plain: Vec<u8> = if !kek_enc.is_empty() {
            let ring = cipher.keyring();
            // kek_enc 用「提交时刻的当前 KEK」加密；从最新到最旧逐个尝试（节点可能落后/追赶）
            let mut resolved: Option<[u8; 32]> = None;
            for k in ring.entries().iter().rev() {
                if let Ok(kk) = dsh_crypto::Cipher::unwrap_master_key(k, kek_enc) {
                    resolved = Some(kk);
                    break;
                }
            }
            match resolved {
                Some(k) => k.to_vec(),
                None => {
                    tracing::error!("rotate: cannot unwrap kek_enc with any known KEK");
                    return;
                }
            }
        } else {
            kek.clone()
        };
        if plain.len() != 32 {
            return;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&plain);
        let ring = cipher.keyring();
        if ring.entries().iter().any(|k| k == &arr) {
            return; // 幂等（重放/多节点）
        }
        let mut new_ring = ring;
        new_ring.push(arr);
        if let Some(p) = &ring_path {
            if let Err(e) = dsh_crypto::save_ring(p, &new_ring) {
                tracing::error!("rotate: persist ring file: {e}");
                return; // 持久化失败不切换内存（保持可解）
            }
        }
        cipher.rotate_master_key(arr);
    }))
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

// ---------------- G1/D35-37：CLI 参数枚举（clap value_enum → core 类型） ----------------

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum PolicyArg {
    Block,
    Warn,
}
impl From<PolicyArg> for dsh_core::model::PublishPolicy {
    fn from(v: PolicyArg) -> Self {
        match v {
            PolicyArg::Block => dsh_core::model::PublishPolicy::Block,
            PolicyArg::Warn => dsh_core::model::PublishPolicy::Warn,
        }
    }
}
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum CascadeArg {
    Auto,
    Manual,
}
impl From<CascadeArg> for dsh_core::model::SharedCascadeMode {
    fn from(v: CascadeArg) -> Self {
        match v {
            CascadeArg::Auto => dsh_core::model::SharedCascadeMode::Auto,
            CascadeArg::Manual => dsh_core::model::SharedCascadeMode::Manual,
        }
    }
}
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum ReadArg {
    Linear,
    Stale,
}
impl From<ReadArg> for dsh_core::model::ReadMode {
    fn from(v: ReadArg) -> Self {
        match v {
            ReadArg::Linear => dsh_core::model::ReadMode::Linear,
            ReadArg::Stale => dsh_core::model::ReadMode::Stale,
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
    // 可信代理（F4）：解析失败直接报错退出
    let trusted_proxies = std::sync::Arc::new(match &cli.trusted_proxy {
        Some(s) => {
            dsh_api::TrustedProxies::parse(s).map_err(|e| format!("--trusted-proxy: {e}"))?
        }
        None => dsh_api::TrustedProxies::empty(),
    });
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
        // N3：ring 文件损坏/解析失败不再静默当空处理 —— 旧密文将不可解，必须告警。
        let ring_entries = match ring_path.as_ref() {
            Some(p) => match dsh_crypto::load_ring(p) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("load ring {}: {e}", p.display());
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // 去重：ring 文件首项即为文件主密钥本身（首次轮换时写入），
        // 直接 extend 会让每次重启代际 +1 且 keyring 含重复 KEK。
        let mut entries = vec![k];
        entries.extend(ring_entries.into_iter().filter(|e| *e != k));
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
        // hub 将被移入 ApiState，先取 sender 供自动回滚广播（G5/D33）
        let hub_sender = hub.sender().clone();
        let mut app = ApiState::with_retention(
            Arc::new(RwLock::new(sm)),
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
            cli.join_token.clone().map(Arc::from),
            trusted_proxies.clone(),
            cli.data_plane_token.clone().map(Arc::from),
        );
        // G1/D35-37：发布策略/级联/读取模式注入
        app.publish.publish_policy = cli.publish_policy.into();
        app.publish.shared_cascade = cli.shared_cascade.into();
        app.read_mode = cli.read_mode.into();
        // G5/D33：灰度自动回滚（dev-single 恒 leader；threshold 百分比 → 比例）
        if cli.gray_rollback_threshold > 0.0 {
            let (_leader_tx, leader_rx) = tokio::sync::watch::channel(true);
            dsh_jobs::spawn_gray_auto_rollback(
                app.sm.clone(),
                None,
                Some(hub_sender),
                app.audit.clone(),
                Box::new(dsh_jobs::LocalHttp5xxProbe),
                cli.gray_rollback_threshold / 100.0,
                std::time::Duration::from_secs(cli.gray_rollback_interval),
                leader_rx,
            );
        }
        spawn_grpc(&cli, app.clone());
        let router = dsh_api::build_router(app);
        let listener = tokio::net::TcpListener::bind(&cli.http_addr).await?;
        eprintln!("dsh --dev-single listening on http://{}", cli.http_addr);
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await?;
        return Ok(());
    }

    // ---------- 集群模式 ----------
    let data_dir = cli.data_dir.clone().ok_or("集群模式需要 --data-dir")?;
    let node_id = cli.node_id.ok_or("集群模式需要 --node-id")?;
    // F3：集群模式强制引导令牌（S2 修复默认生效）——否则任意网络可达者可注册 learner
    // 拉走全量 Raft 日志（含密码哈希/会话哈希/密文）。dev-single 无 raft，不要求。
    let join_token = cli
        .join_token
        .clone()
        .ok_or("集群模式需要 --join-token（join 端点鉴权；集群内所有节点传相同值）")?;
    // S5：raft 内部 RPC 端口同样要求共享令牌（纵深防御；防伪造 vote/append 制造选举抖动）
    let raft_token = cli
        .raft_token
        .clone()
        .ok_or("集群模式需要 --raft-token（raft RPC 鉴权；集群内所有节点传相同值）")?;

    let storage = RedbStorage::open(&data_dir)?;
    let db = storage.raw_db();
    let sm = Arc::new(RwLock::new(StateMachine::new(Box::new(storage))));
    // 集群模式挂主密钥轮换钩子：Raft apply 到 RotateMasterKey 时更新本节点 keyring 并持久化 ring 文件
    // （dev-single 不挂——它走 handler 的本地轮换逻辑，先持久化后切换）。
    let sm_store = Arc::new(StateMachineStore::new_with_rotation(
        sm.clone(),
        db.clone(),
        cluster_rotation_hook(cli.master_key_file.as_deref(), cipher.clone()),
    ));
    // 重启恢复：raft-meta 非空说明该节点已有持久化状态 → 无需 --bootstrap/--join，自动 resume
    let has_state = sm_store.has_persisted_state();
    if !cli.bootstrap && cli.join.is_none() && !has_state {
        return Err("集群模式需要 --bootstrap、--join 或已有数据目录".into());
    }
    // 集群 watch：raft apply 事件 → hub（SSE）
    hub.spawn_raft_forward(sm_store.clone());
    let log_store = LogStore::new(db.clone());

    let node_info = RaftNodeInfo {
        grpc_addr: cli.grpc_addr.clone(),
        http_addr: cli.http_addr.clone(),
        raft_addr: cli.raft_addr.clone(),
    };
    let network = HttpNetworkFactory::with_token(Some(raft_token.clone()));
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
        join_cluster(
            &raft,
            node_id,
            node_info.clone(),
            join_url,
            Some(join_token.as_str()),
        )
        .await?;
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
        scheduler.spawn(sm.clone(), leader_rx.clone());
        // G5/D33：灰度自动回滚（可选，仅 leader；threshold 为百分比 → 转比例）
        if cli.gray_rollback_threshold > 0.0 {
            let audit = dsh_observability::AuditLog::new(sm.clone(), Some(raft.clone()));
            dsh_jobs::spawn_gray_auto_rollback(
                sm.clone(),
                Some(raft.clone()),
                Some(hub.sender().clone()),
                audit,
                Box::new(dsh_jobs::LocalHttp5xxProbe),
                cli.gray_rollback_threshold / 100.0,
                std::time::Duration::from_secs(cli.gray_rollback_interval),
                leader_rx,
            );
        }
    }

    // Raft RPC 服务（raft_addr）
    let raft_state =
        RaftServerState::with_token(raft.clone(), Some(Arc::from(raft_token.as_str())));
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
    let mut app = ApiState::with_retention(
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
        Some(Arc::from(join_token.as_str())),
        trusted_proxies,
        cli.data_plane_token.clone().map(Arc::from),
    );
    // G1/D35-37：发布策略/级联/读取模式注入
    app.publish.publish_policy = cli.publish_policy.into();
    app.publish.shared_cascade = cli.shared_cascade.into();
    app.read_mode = cli.read_mode.into();
    spawn_grpc(&cli, app.clone());
    let router = dsh_api::build_router(app);
    let listener = tokio::net::TcpListener::bind(&cli.http_addr).await?;
    eprintln!(
        "dsh node {node_id} listening on http://{} (raft {})",
        cli.http_addr, cli.raft_addr
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
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
