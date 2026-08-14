//! Raft 类型配置（模块 03 §2）。

use std::io::Cursor;

use dsh_core::command::Command;
use openraft::declare_raft_types;
use serde::{Deserialize, Serialize};

pub type NodeId = u64;

/// 节点信息。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub grpc_addr: String,
    pub http_addr: String,
    pub raft_addr: String,
}

impl std::fmt::Display for NodeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.raft_addr)
    }
}

// NodeInfo 满足 NodeEssential（Default/Debug/Clone/PartialEq/Eq + serde），openraft::Node 由 blanket impl 提供。

// 客户端写响应：apply 产出的版本号（非发布命令为 0）。
// Err 携带状态机 apply 错误（I7 会话冲突/发布校验等），随 Raft 响应返回给客户端 ——
// 之前 R=u64 会把 apply 错误吞掉（日志警告后返回 0），跨节点错误码无法传播。
declare_raft_types!(
    pub TypeConfig:
        D = Command,
        R = Result<u64, dsh_core::Error>,
        NodeId = NodeId,
        Node = NodeInfo,
        SnapshotData = Cursor<Vec<u8>>,
);
