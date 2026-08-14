//! HTTP 网络传输（多进程/生产，模块 03）：Raft RPC 经 HTTP+JSON。
//! 服务端：RaftHttpServer（axum，/raft/* 端点）；客户端：HttpNetwork（reqwest）。
//! 说明：M1 简化——错误以 500+JSON 返回，客户端映射为 Network 错误（重试）；
//! 快照分块由 openraft 默认 full_snapshot 按 chunk 调用 install_snapshot。

use std::io;

use openraft::error::{NetworkError, RPCError};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use serde::de::DeserializeOwned;

use crate::types::{NodeId, NodeInfo, TypeConfig};

pub type RaftHandle = openraft::Raft<TypeConfig>;

/// HTTP 网络客户端（发送给指定目标）。
pub struct HttpNetwork {
    base: String,
    client: reqwest::Client,
}

impl HttpNetwork {
    async fn post<T: serde::Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, RPCError<NodeId, NodeInfo, RaftErrorPlaceholder>> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        if !resp.status().is_success() {
            let e = io::Error::other(format!("raft rpc {} -> {}", path, resp.status()));
            return Err(RPCError::Network(NetworkError::new(&e)));
        }
        resp.json()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }
}

/// 占位错误类型（RPCError 的 E 参数；实际错误以 Network 变体返回）。
pub type RaftErrorPlaceholder = openraft::error::RaftError<NodeId>;

impl RaftNetwork<TypeConfig> for HttpNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, NodeInfo, RaftErrorPlaceholder>>
    {
        self.post("/raft/append-entries", &rpc).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, NodeInfo, RaftErrorPlaceholder>> {
        self.post("/raft/vote", &rpc).await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<
            NodeId,
            NodeInfo,
            openraft::error::RaftError<NodeId, openraft::error::InstallSnapshotError>,
        >,
    > {
        let resp: InstallSnapshotResponse<NodeId> = self
            .post("/raft/install-snapshot", &rpc)
            .await
            .map_err(|e| match e {
                RPCError::Network(ne) => RPCError::Network(ne),
                _ => RPCError::Network(NetworkError::new(&io::Error::other("rpc error"))),
            })?;
        Ok(resp)
    }
}

/// HTTP 网络工厂。
#[derive(Clone, Default)]
pub struct HttpNetworkFactory {
    client: reqwest::Client,
}

impl HttpNetworkFactory {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl RaftNetworkFactory<TypeConfig> for HttpNetworkFactory {
    type Network = HttpNetwork;

    async fn new_client(&mut self, target: NodeId, node: &NodeInfo) -> Self::Network {
        let _ = target;
        HttpNetwork {
            base: format!("http://{}", node.raft_addr),
            client: self.client.clone(),
        }
    }
}
