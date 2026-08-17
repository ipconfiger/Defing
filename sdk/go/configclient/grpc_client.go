// Package configclient — Defing Go SDK（gRPC 数据面客户端）。
// 端点可携带 gRPC 地址（Endpoint{GRPC}）；纯 HTTP 端点走 HTTP/SSE（降级通道）。
// 依赖生成代码：configv1（protoc-gen-go，见 configv1/）。
package configclient

import (
	"context"
	"io"
	"sync"
	"time"

	configv1 "github.com/defing/config-go/configv1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
)

// Endpoint：HTTP 必填（failover 降级），GRPC 可选（优先走 gRPC 数据面 :8383）。
type Endpoint struct {
	HTTP string
	GRPC string
}

// 固定超时/重连参数（跨语言对齐见 docs/design-modules/12-sdk.md）
const (
	// gRPC 流重连退避：1s 固定（MVP；与 TS 对齐）
	grpcReconnectDelay = time.Second
)

// GrpcClient — gRPC 数据面客户端（Get/GetItem/Watch/ListMembers）。
type GrpcClient struct {
	stub     configv1.ConfigServiceClient
	conn     *grpc.ClientConn
	token    string
	instance string
	labels   map[string]string
	mu       sync.Mutex
}

// NewGrpc 建立到单个 gRPC 端点的客户端；token 为数据面令牌（--data-plane-token）。
func NewGrpc(grpcAddr, token string) (*GrpcClient, error) {
	return newGrpc(grpcAddr, token, "", nil)
}

// NewGrpcWithIdentity 建立带灰度发布身份的 gRPC 数据面客户端（G3/D26）。
// instance 为稳定身份键（如 Pod 名/部署单元 ID）；labels 为灰度标签（如 zone=cn-north-1）。
func NewGrpcWithIdentity(grpcAddr, token, instance string, labels map[string]string) (*GrpcClient, error) {
	return newGrpc(grpcAddr, token, instance, labels)
}

func newGrpc(grpcAddr, token, instance string, labels map[string]string) (*GrpcClient, error) {
	conn, err := grpc.NewClient(grpcAddr, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return nil, err
	}
	return &GrpcClient{
		stub:     configv1.NewConfigServiceClient(conn),
		conn:     conn,
		token:    token,
		instance: instance,
		labels:   labels,
	}, nil
}

// ctx 以调用方 ctx 为基座，附加数据面 token（F-SDK：此前用 context.Background 导致
// Watch 无法被取消、goroutine 泄漏）。
func (g *GrpcClient) ctx(ctx context.Context) context.Context {
	if g.token != "" {
		ctx = metadata.AppendToOutgoingContext(ctx, "authorization", "Bearer "+g.token)
	}
	return ctx
}

func valueFromProto(v *configv1.Value) any {
	if v == nil {
		return nil
	}
	switch v.GetType() {
	case configv1.ValueType_STRING:
		return v.GetStrValue()
	case configv1.ValueType_INT:
		return v.GetIntValue()
	case configv1.ValueType_FLOAT:
		return v.GetFloatValue()
	case configv1.ValueType_BOOL:
		return v.GetBoolValue()
	case configv1.ValueType_JSON:
		return v.GetJsonValue()
	case configv1.ValueType_ARRAY:
		return v.GetListValue().GetValues()
	case configv1.ValueType_SECRET:
		// secret 数据面恒脱敏（服务器 grpc.rs:58-61）：掩码展示
		return "***"
	default:
		// 未知类型（协议扩展后旧客户端）：显式 nil 而非静默误判为 secret
		return nil
	}
}

func snapshotFromProto(s *configv1.ConfigSnapshot) *Snapshot {
	groups := make(map[string]map[string]any, len(s.GetGroups()))
	for g, gd := range s.GetGroups() {
		m := make(map[string]any, len(gd.GetItems()))
		for k, v := range gd.GetItems() {
			m[k] = valueFromProto(v)
		}
		groups[g] = m
	}
	return &Snapshot{
		Project:          s.GetProject(),
		Branch:           s.GetBranch(),
		Version:          s.GetVersion(),
		StructureVersion: s.GetStructureVersion(),
		Groups:           groups,
		Gray:             s.GetGray(),
		ResolvedVersion:  s.GetResolvedVersion(),
	}
}

// Get 拉取 (project, branch) 快照；version=0 为活动版本。ctx 贯穿（取消/超时生效）。
func (g *GrpcClient) Get(ctx context.Context, project, branch string, version int64) (*Snapshot, error) {
	resp, err := g.stub.GetConfig(g.ctx(ctx), &configv1.GetConfigRequest{
		Project: project, Branch: branch, Version: version,
		InstanceId: g.instance,
		Labels:     g.labels,
	})
	if err != nil {
		return nil, err
	}
	return snapshotFromProto(resp), nil
}

// GetItem 获取单个 item 值。
func (g *GrpcClient) GetItem(ctx context.Context, project, branch, group, key string, version int64) (any, error) {
	snap, err := g.Get(ctx, project, branch, version)
	if err != nil {
		return nil, err
	}
	if gd, ok := snap.Groups[group]; ok {
		if v, ok := gd[key]; ok {
			return v, nil
		}
	}
	return nil, nil
}

// ListMembers 集群成员（dev-single → FailedPrecondition）。
func (g *GrpcClient) ListMembers(ctx context.Context) ([]Member, error) {
	resp, err := g.stub.ListMembers(g.ctx(ctx), &configv1.ListMembersRequest{})
	if err != nil {
		return nil, err
	}
	out := make([]Member, 0, len(resp.GetMembers()))
	for _, m := range resp.GetMembers() {
		out = append(out, Member{
			NodeID:         m.GetNodeId(),
			GrpcAddr:       m.GetGrpcAddr(),
			HTTPAddr:       m.GetHttpAddr(),
			IsLeader:       m.GetIsLeader(),
			IsVoter:        m.GetIsVoter(),
			CommittedIndex: m.GetCommittedIndex(),
		})
	}
	return out, nil
}

// Watch 订阅 (project, branch) 发布事件；断线以 after_version 续传重连；
// 阻塞直至 ctx 取消或 stop 关闭。事件含 SnapshotRequired 标志。
// F-SDK：流的创建使用调用方 ctx——ctx 取消会关闭流并返回 ctx.Err()（不再泄漏 goroutine）。
func (g *GrpcClient) Watch(ctx context.Context, project, branch string, afterVersion int64, listener func(WatchEvent)) error {
	for {
		// B1 契约：订阅/重连先做一次 snapshot 拉取，重锚版本游标（灰度 publish/abort 不写 v/ 记录，重放不含）
		if snap, serr := g.Get(ctx, project, branch, 0); serr == nil && snap.Version > afterVersion {
			afterVersion = snap.Version
		}
		stream, err := g.stub.Watch(g.ctx(ctx), &configv1.WatchRequest{
			Project: project, Branch: branch, AfterVersion: afterVersion,
		})
		if err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			time.Sleep(grpcReconnectDelay)
			continue
		}
		for {
			e, err := stream.Recv()
			if err == io.EOF {
				break
			}
			if err != nil {
				if ctx.Err() != nil {
					return ctx.Err()
				}
				time.Sleep(grpcReconnectDelay)
				break // 重连（after_version 续传）
			}
			// 游标只增不减：灰度事件（gray=true）可携带 ≤ afterVersion 的版本（promote/abort 补发），
			// 但事件本身永不按版本过滤（gRPC 无客户端侧版本过滤，服务端已按 after_version 重放）。
			if e.GetVersion() > afterVersion {
				afterVersion = e.GetVersion()
			}
			changes := make([]Change, 0, len(e.GetChanges()))
			for _, c := range e.GetChanges() {
				kind := "upsert"
				if c.GetKind() == configv1.ChangeKind_DELETE {
					kind = "delete"
				}
				changes = append(changes, Change{
					Group: c.GetGroup(), Key: c.GetKey(), Kind: kind,
					NewValue: valueFromProto(c.GetNewValue()),
				})
			}
			ty := ""
			switch e.GetType() {
			case configv1.EventType_VALUE_PUBLISH:
				ty = "value_publish"
			case configv1.EventType_STRUCTURE_PUBLISH:
				ty = "structure_publish"
			case configv1.EventType_SHARED_CASCADE:
				ty = "shared_cascade"
			case configv1.EventType_ROLLBACK:
				ty = "rollback"
			}
			listener(WatchEvent{
				Version:          e.GetVersion(),
				Ty:               ty,
				StructureVersion: e.GetStructureVersion(),
				Comment:          e.GetComment(),
				RequestID:        e.GetRequestId(),
				Changes:          changes,
				SnapshotRequired: e.GetSnapshotRequired(),
				Gray:             e.GetGray(),
			})
		}
	}
}

// Close 关闭底层连接。
func (g *GrpcClient) Close() error {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.conn != nil {
		err := g.conn.Close()
		g.conn = nil
		return err
	}
	return nil
}
