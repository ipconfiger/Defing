//! Defing 配置服务 —— openraft 集成（模块 03）。
//! 本版：进程内直连网络（测试/联调）；存储基于 dsh-storage RocksDB。

pub mod http_network;
pub mod network;
pub mod raft;
pub mod raft_http_server;
pub mod store;
pub mod types;

pub use http_network::{HttpNetwork, HttpNetworkFactory};
pub use network::{Network, NetworkFactory, RaftHandle};
pub use raft::{
    client_write, dev_config, initialize_single, leader_http_addr, new_raft_node, try_client_write,
    wait_for_leader, wait_until, write_command, WriteError, WriteOutcome,
};
pub use raft_http_server::{raft_router, RaftServerState};
pub use store::{LogStore, StateMachineStore};
pub use types::{NodeId, NodeInfo, TypeConfig};
