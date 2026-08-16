//! gRPC 数据面（模块 05 / proto config.v1）：GetConfig / GetItem / Watch / ListMembers。
//! 只读 + watch；鉴权：metadata `authorization: Bearer <api_token>`（静态表，MVP；未配置 token 时开放）。

use std::pin::Pin;

use dsh_core::diff::compute_diff;
use dsh_core::model::{
    BranchName, ChangeKind as CoreChangeKind, EventType as CoreEventType, ProjectId, SnapshotMap,
    Value as CoreValue,
};
use futures::Stream;
use tonic::{Request, Response, Status};

use crate::ApiState;

tonic::include_proto!("config.v1");

/// gRPC 服务实现（只读；复用 HTTP 的 ApiState）。
#[derive(Clone)]
pub struct ConfigGrpcService {
    pub state: ApiState,
}

/// 数据面鉴权拦截器（metadata `authorization: Bearer <token>`；未配置 token 时放行）。
pub fn data_plane_interceptor(
    data_token: Option<String>,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, Status> + Clone + Send + 'static {
    move |req: tonic::Request<()>| {
        if let Some(token) = &data_token {
            let ok = req
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|a| a.strip_prefix("Bearer "))
                .map(|t| t == token.as_str())
                .unwrap_or(false);
            if !ok {
                return Err(Status::unauthenticated("invalid data-plane token"));
            }
        }
        Ok(req)
    }
}

// ---------------- proto ↔ 内部模型转换 ----------------

fn value_to_proto(v: &CoreValue) -> Value {
    let mut masked = false;
    let data = match v {
        CoreValue::String(s) => value::Data::StrValue(s.clone()),
        CoreValue::Int(i) => value::Data::IntValue(*i),
        CoreValue::Float(f) => value::Data::FloatValue(*f),
        CoreValue::Bool(b) => value::Data::BoolValue(*b),
        CoreValue::Json(s) => value::Data::JsonValue(s.clone()),
        CoreValue::Array(items) => value::Data::ListValue(StringList {
            values: items.clone(),
        }),
        // 数据面不解密 secret：脱敏 + masked 标记（design-v2 §7.6）
        CoreValue::Secret(_) => {
            masked = true;
            value::Data::StrValue("***".into())
        }
    };
    Value {
        r#type: value_type_to_proto(&v.value_type()),
        data: Some(data),
        masked,
    }
}

fn value_type_to_proto(t: &dsh_core::model::ValueType) -> i32 {
    match t {
        dsh_core::model::ValueType::String => ValueType::String.into(),
        dsh_core::model::ValueType::Int => ValueType::Int.into(),
        dsh_core::model::ValueType::Float => ValueType::Float.into(),
        dsh_core::model::ValueType::Bool => ValueType::Bool.into(),
        dsh_core::model::ValueType::Json => ValueType::Json.into(),
        dsh_core::model::ValueType::Array => ValueType::Array.into(),
        dsh_core::model::ValueType::Secret => ValueType::Secret.into(),
    }
}

fn snapshot_to_proto(p: &str, b: &str, snap: &dsh_core::ConfigSnapshot) -> ConfigSnapshot {
    let groups = snap
        .groups
        .iter()
        .map(|(g, items)| {
            let item_map = items
                .iter()
                .map(|(k, v)| (k.clone(), value_to_proto(v)))
                .collect();
            (g.clone(), GroupData { items: item_map })
        })
        .collect();
    ConfigSnapshot {
        project: p.to_string(),
        branch: b.to_string(),
        version: snap.version as i64,
        structure_version: snap.structure_version as i64,
        groups,
    }
}

fn diff_to_changes(diff: Vec<dsh_core::model::DiffEntry>) -> Vec<Change> {
    diff.into_iter()
        .map(|d| Change {
            group: d.group,
            key: d.key,
            kind: match d.kind {
                CoreChangeKind::Upsert => ChangeKind::Upsert.into(),
                CoreChangeKind::Delete => ChangeKind::Delete.into(),
            },
            new_value: d.new_value.as_ref().map(value_to_proto),
        })
        .collect()
}

fn event_type_to_proto(t: CoreEventType) -> i32 {
    match t {
        CoreEventType::ValuePublish => EventType::ValuePublish.into(),
        CoreEventType::StructurePublish => EventType::StructurePublish.into(),
        CoreEventType::SharedCascade => EventType::SharedCascade.into(),
        CoreEventType::Rollback => EventType::Rollback.into(),
    }
}

// ---------------- 服务实现 ----------------

#[tonic::async_trait]
impl config_service_server::ConfigService for ConfigGrpcService {
    async fn get_config(
        &self,
        req: Request<GetConfigRequest>,
    ) -> Result<Response<ConfigSnapshot>, Status> {
        let r = req.into_inner();
        let sm = self
            .state
            .sm
            .read()
            .map_err(|_| Status::internal("sm lock"))?;
        let snap = sm
            .get_config(
                &ProjectId(r.project.clone()),
                &BranchName(r.branch.clone()),
                r.version.max(0) as u64,
            )
            .map_err(map_err)?;
        Ok(Response::new(snapshot_to_proto(
            &r.project, &r.branch, &snap,
        )))
    }

    async fn get_item(&self, req: Request<GetItemRequest>) -> Result<Response<ItemValue>, Status> {
        let r = req.into_inner();
        let sm = self
            .state
            .sm
            .read()
            .map_err(|_| Status::internal("sm lock"))?;
        let snap = sm
            .get_config(
                &ProjectId(r.project.clone()),
                &BranchName(r.branch.clone()),
                r.version.max(0) as u64,
            )
            .map_err(map_err)?;
        let value = snap
            .groups
            .get(&r.group)
            .and_then(|items| items.get(&r.key))
            .ok_or_else(|| Status::not_found(format!("{}/{}/{}", r.project, r.group, r.key)))?;
        Ok(Response::new(ItemValue {
            group: r.group,
            key: r.key,
            value: Some(value_to_proto(value)),
        }))
    }

    type WatchStream = Pin<Box<dyn Stream<Item = Result<WatchEvent, Status>> + Send>>;

    async fn watch(
        &self,
        req: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let r = req.into_inner();
        let project = r.project.clone();
        let branch = r.branch.clone();
        let after: i64 = r.after_version;

        // 1) 重放 after_version 之后的历史版本（合成为事件；changes 由相邻快照 diff 得出）
        let mut replay: Vec<WatchEvent> = Vec::new();
        let mut snapshot_required = false;
        if after > 0 {
            let pid = ProjectId(project.clone());
            let bname = BranchName(branch.clone());
            let sm = self
                .state
                .sm
                .read()
                .map_err(|_| Status::internal("sm lock"))?;
            let hist = sm
                .version_history(&pid, &bname)
                .map_err(|e| Status::internal(e.to_string()))?;
            // D-PRUNED：断线起点已被版本保留策略裁剪（最早保留版本 > after 且未到活动版本）
            // → 客户端缓存已失效，发 snapshot_required 并关流（不再静默丢事件）。
            if let (Some(min), Some(active)) =
                (hist.first().map(|r| r.no), hist.last().map(|r| r.no))
            {
                if (after as u64) < min && (after as u64) < active {
                    snapshot_required = true;
                }
            }
            let mut prev: SnapshotMap = SnapshotMap::new();
            for rec in hist {
                if (rec.no as i64) <= after {
                    continue;
                }
                let cur = sm
                    .snapshot_of(&pid, &bname, rec.no)
                    .map_err(|e| Status::internal(e.to_string()))?;
                let diff = compute_diff(&prev, &cur);
                prev = cur;
                // D-TYPE：事件类型保真（结构发布/级联不再被标为 value_publish；旧日志回退推断）
                let ty = rec.event_ty.map(event_type_to_proto).unwrap_or_else(|| {
                    if rec.rollback_of.is_some() {
                        EventType::Rollback.into()
                    } else {
                        EventType::ValuePublish.into()
                    }
                });
                replay.push(WatchEvent {
                    version: rec.no as i64,
                    r#type: ty,
                    structure_version: rec.structure_version as i64,
                    comment: rec.comment,
                    request_id: String::new(),
                    changes: diff_to_changes(diff),
                    snapshot_required: false,
                });
            }
        }

        // 2) 实时事件（与重放版本去重）；慢消费者（广播缓冲溢出）→ 发 snapshot_required 并关流
        let mut rx = self.state.hub.subscribe();
        let stream = async_stream::stream! {
            let mut last = after;
            // D-PRUNED：起点被裁剪 → 直接发 snapshot_required 并结束（客户端重拉全量后重订阅）
            if snapshot_required {
                yield Ok(WatchEvent {
                    version: last,
                    r#type: EventType::ValuePublish.into(),
                    structure_version: 0,
                    comment: "snapshot required (start pruned)".into(),
                    request_id: String::new(),
                    changes: vec![],
                    snapshot_required: true,
                });
                return;
            }
            for e in replay {
                last = e.version;
                yield Ok(e);
            }
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        if e.project.as_str() == project && e.branch.as_str() == branch
                            && (e.version as i64) > last
                        {
                            last = e.version as i64;
                            yield Ok(WatchEvent {
                                version: e.version as i64,
                                r#type: event_type_to_proto(e.ty),
                                structure_version: e.structure_version as i64,
                                comment: e.comment,
                                request_id: e.request_id,
                                changes: diff_to_changes(e.changes),
                                snapshot_required: false,
                            });
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // 消费不及：客户端缓存已失效，提示重拉全量并关流（design §6.3）
                        yield Ok(WatchEvent {
                            version: last,
                            r#type: EventType::ValuePublish.into(),
                            structure_version: 0,
                            comment: "slow consumer: snapshot required".into(),
                            request_id: String::new(),
                            changes: vec![],
                            snapshot_required: true,
                        });
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_members(
        &self,
        _req: Request<ListMembersRequest>,
    ) -> Result<Response<ListMembersResponse>, Status> {
        let raft = self
            .state
            .raft
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("not in cluster mode"))?;
        let m = raft.metrics().borrow().clone();
        let leader = m.current_leader;
        let voter_ids: Vec<u64> = m.membership_config.membership().voter_ids().collect();
        let committed = m.last_log_index.unwrap_or(0) as i64;
        let members = m
            .membership_config
            .membership()
            .nodes()
            .map(|(id, n)| Member {
                node_id: id.to_string(),
                grpc_addr: n.grpc_addr.clone(),
                http_addr: n.http_addr.clone(),
                is_leader: Some(*id) == leader,
                is_voter: voter_ids.contains(id),
                committed_index: committed,
            })
            .collect();
        Ok(Response::new(ListMembersResponse { members }))
    }
}

fn map_err(e: dsh_core::Error) -> Status {
    match e.kind {
        dsh_core::ErrorKind::NotFound => Status::not_found(e.message),
        dsh_core::ErrorKind::Validation => Status::invalid_argument(e.message),
        dsh_core::ErrorKind::SessionInUse | dsh_core::ErrorKind::SessionExpired => {
            Status::unauthenticated(e.message)
        }
        _ => Status::internal(e.message),
    }
}
