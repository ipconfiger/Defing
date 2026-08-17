// Defing ConfigClient — TypeScript SDK gRPC 数据面（config.v1.ConfigService）。
// 动态加载 proto（@grpc/proto-loader），无需代码生成；与 HTTP/SSE 通道 API 形状一致。
// 鉴权：metadata `authorization: Bearer <token>`（--data-plane-token 配置时）。

import * as grpc from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';

export interface GrpcOptions {
  grpc: string;
  token?: string;
  instanceId?: string;
  labels?: Record<string, string>;
}

/** 带错误码的错误（index.ts 包装为 ConfigError；code 形如 GRPC_<status>）。 */
export function grpcError(code: string, message: string): Error {
  const e = new Error(message) as Error & { code: string };
  e.code = code;
  return e;
}

// proto 位于仓库根 proto/config.v1.proto（相对本文件：sdk/ts/src → ../../..）
const PROTO_PATH = new URL('../../../proto/config.v1.proto', import.meta.url).pathname;

const packageDefinition = protoLoader.loadSync(PROTO_PATH, {
  keepCase: true,
  longs: Number,
  defaults: true,
  oneofs: true,
  enums: Number,
});
const proto = (grpc.loadPackageDefinition(packageDefinition) as any).config.v1;

/** proto Value → 普通 JS 值（与 HTTP 快照形状一致：纯值；secret 脱敏为 "***"）。 */
function valueFromProto(v: any): unknown {
  if (!v) return null;
  switch (v.type) {
    case proto.ValueType.STRING:
      return v.str_value;
    case proto.ValueType.INT:
      return v.int_value;
    case proto.ValueType.FLOAT:
      return v.float_value;
    case proto.ValueType.BOOL:
      return v.bool_value;
    case proto.ValueType.JSON:
      return v.json_value;
    case proto.ValueType.ARRAY:
      return v.list_value?.values ?? [];
    case proto.ValueType.SECRET:
    default:
      return v.masked ? '***' : v.str_value;
  }
}

function snapshotFromProto(s: any) {
  const groups: Record<string, Record<string, unknown>> = {};
  for (const [g, gd] of Object.entries(s.groups ?? {})) {
    groups[g] = {};
    for (const [k, v] of Object.entries((gd as any).items ?? {})) {
      groups[g][k] = valueFromProto(v);
    }
  }
  return {
    project: s.project,
    branch: s.branch,
    version: s.version,
    structure_version: s.structure_version,
    groups,
    gray: !!s.gray,
    resolved_version: s.resolved_version,
  };
}

function changesFromProto(changes: any[]) {
  return (changes ?? []).map((c) => ({
    group: c.group,
    key: c.key,
    kind: c.kind === proto.ChangeKind.DELETE ? 'delete' : 'upsert',
    new_value: c.new_value ? valueFromProto(c.new_value) : undefined,
  }));
}

function eventFromProto(e: any) {
  return {
    project: '',
    branch: '',
    version: e.version,
    ty: ['', 'value_publish', 'structure_publish', 'shared_cascade', 'rollback'][e.type] ?? 'value_publish',
    structure_version: e.structure_version,
    comment: e.comment,
    request_id: e.request_id,
    changes: changesFromProto(e.changes),
    snapshot_required: !!e.snapshot_required,
    gray: !!e.gray,
  };
}

/** gRPC ConfigService 客户端（端点失败抛 ConfigError，调用方按 failover 切换）。 */
export class GrpcConfigClient {
  private client: any;
  private meta: grpc.Metadata;
  private instanceId?: string;
  private labels?: Record<string, string>;

  constructor(opts: GrpcOptions) {
    this.client = new proto.ConfigService(opts.grpc, grpc.credentials.createInsecure());
    this.meta = new grpc.Metadata();
    if (opts.token) this.meta.add('authorization', 'Bearer ' + opts.token);
    this.instanceId = opts.instanceId;
    this.labels = opts.labels;
  }

  getConfig(project: string, branch: string, version = 0): Promise<any> {
    return new Promise((resolve, reject) => {
      const req: Record<string, unknown> = { project, branch, version };
      if (this.instanceId) req.instance_id = this.instanceId;
      if (this.labels) req.labels = this.labels;
      this.client.getConfig(req, this.meta, (err: any, resp: any) => {
        if (err) return reject(grpcError('GRPC_' + err.code, err.details || err.message));
        resolve(snapshotFromProto(resp));
      });
    });
  }

  async getItem(project: string, branch: string, group: string, key: string, version = 0): Promise<unknown | undefined> {
    const s = await this.getConfig(project, branch, version);
    return s.groups?.[group]?.[key];
  }

  listMembers(): Promise<any[]> {
    return new Promise((resolve, reject) => {
      this.client.listMembers({}, this.meta, (err: any, resp: any) => {
        if (err) return reject(grpcError('GRPC_' + err.code, err.details || err.message));
        resolve(resp.members ?? []);
      });
    });
  }

  /**
   * 订阅发布事件（服务端流）。断线自动以 after_version=last 续传重连；
   * snapshot_required 事件 → 回调携带标志，调用方应重拉全量。
   */
  watch(project: string, branch: string, listener: (e: any) => void, signal?: AbortSignal): void {
    let after = 0;
    let lastEmitted = 0;
    const connect = () => {
      if (signal?.aborted) return;
      // B1 契约：订阅/重连先做一次 snapshot 拉取，重锚版本游标（灰度 publish/abort 不写 v/ 记录，重放不含）
      this.getConfig(project, branch, 0)
        .then((snap: any) => {
          if (snap.version) after = Math.max(after, snap.version);
          if (signal?.aborted) return;
          const call = this.client.watch({ project, branch, after_version: after }, this.meta);
          call.on('data', (e: any) => {
            after = Math.max(after, e.version);
            if (!e.gray && e.version <= lastEmitted) return; // F-SDK：重放/重连重复投递去重；灰度事件永不过滤
            if (!e.gray) lastEmitted = e.version;
            listener(eventFromProto(e));
          });
          call.on('error', () => schedule());
          call.on('end', () => schedule());
          signal?.addEventListener('abort', () => call.cancel(), { once: true });
        })
        .catch(() => schedule());
    };
    let timer: ReturnType<typeof setTimeout> | null = null;
    const schedule = () => {
      if (signal?.aborted) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(connect, 1000);
    };
    connect();
  }

  close(): void {
    this.client.close();
  }
}
