//! 进程内直连网络（模块 03：测试/联调用）。
//! 注意：openraft 0.9 的 RaftNetwork/RaftNetworkFactory 使用原生 async fn in trait（AFIT），
//! 实现时必须用原生 async fn，不能使用 async-trait 宏。
#![allow(clippy::result_large_err)] // RPCError Err 变体较大（上游类型）

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, RwLock};

use openraft::error::{NetworkError, RPCError, RaftError, RemoteError};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};

use crate::types::{NodeId, NodeInfo, TypeConfig};

pub type RaftHandle = openraft::Raft<TypeConfig>;

/// 直连网络（发送给指定目标节点）。
pub struct Network {
    target: NodeId,
    node: NodeInfo,
    peers: Arc<RwLock<HashMap<NodeId, RaftHandle>>>,
}

impl Network {
    fn peer<E>(&self) -> Result<RaftHandle, RPCError<NodeId, NodeInfo, E>>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        self.peers
            .read()
            .expect("peers lock")
            .get(&self.target)
            .cloned()
            .ok_or_else(|| {
                let e = io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("peer {} not found", self.target),
                );
                RPCError::Network(NetworkError::new(&e))
            })
    }

    fn wrap<E>(&self, e: E) -> RPCError<NodeId, NodeInfo, E>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        RPCError::RemoteError(RemoteError {
            target: self.target,
            target_node: Some(self.node.clone()),
            source: e,
        })
    }
}

impl RaftNetwork<TypeConfig> for Network {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, NodeInfo, RaftError<NodeId>>> {
        let peer = self.peer()?;
        peer.append_entries(rpc).await.map_err(|e| self.wrap(e))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, NodeInfo, RaftError<NodeId>>> {
        let peer = self.peer()?;
        peer.vote(rpc).await.map_err(|e| self.wrap(e))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, NodeInfo, RaftError<NodeId, openraft::error::InstallSnapshotError>>,
    > {
        let peer = self.peer()?;
        peer.install_snapshot(rpc).await.map_err(|e| self.wrap(e))
    }
}

/// 网络工厂：持有节点表，为每个目标创建直连网络。
#[derive(Clone, Default)]
pub struct NetworkFactory {
    peers: Arc<RwLock<HashMap<NodeId, RaftHandle>>>,
}

impl NetworkFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, id: NodeId, raft: RaftHandle) {
        self.peers.write().expect("peers lock").insert(id, raft);
    }

    pub fn remove(&self, id: NodeId) {
        self.peers.write().expect("peers lock").remove(&id);
    }

    pub fn get(&self, id: &NodeId) -> Option<RaftHandle> {
        self.peers.read().expect("peers lock").get(id).cloned()
    }
}

impl RaftNetworkFactory<TypeConfig> for NetworkFactory {
    type Network = Network;

    async fn new_client(&mut self, target: NodeId, node: &NodeInfo) -> Self::Network {
        Network {
            target,
            node: node.clone(),
            peers: self.peers.clone(),
        }
    }
}
