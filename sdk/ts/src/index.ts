// Defing ConfigClient — TypeScript SDK。
// 运行时说明（F-SDK 修正）：gRPC 通道依赖 @grpc/grpc-js + proto-loader（fs/http2）→ **Node-only**；
// 浏览器环境仅 HTTP/SSE 通道可用（fetch/EventSource），且需 TS 感知 bundler 编译本入口（无 dist 产物）。
// 端点池 failover：连接失败自动切换下一个端点（指数退避）；HTTP 4xx/5xx 视为确定性错误不切换端点。

export interface Change {
  group: string;
  key: string;
  kind: 'upsert' | 'delete';
  new_value?: unknown;
}

export interface WatchEvent {
  project: string;
  branch: string;
  version: number;
  ty: string;
  structure_version: number;
  comment: string;
  request_id: string;
  changes: Change[];
  snapshot_required?: boolean;
  gray?: boolean;
}

export interface Snapshot {
  project: string;
  branch: string;
  version: number;
  structure_version: number;
  groups: Record<string, Record<string, unknown>>;
  gray?: boolean;
  resolved_version?: number;
}

export interface Member {
  node_id: string;
  grpc_addr: string;
  http_addr: string;
  is_leader: boolean;
  is_voter: boolean;
  committed_index: number;
}

/** 端点：纯字符串=HTTP 端点；对象可含 grpc 地址（优先走 gRPC）。 */
export type Endpoint = string | { http?: string; grpc?: string };

export class ConfigError extends Error {
  public code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

export interface ConfigClientOptions {
  /** 数据面 gRPC/HTTP 令牌（metadata authorization: Bearer <token>） */
  token?: string;
  /** 灰度稳定身份键（如 Pod 名/部署单元 ID；非空时经 gRPC instance_id / HTTP X-Dsh-Instance 上报） */
  instance?: string;
  /** 灰度标签（如 zone=cn-north-1；空/缺省 = 不参与标签匹配） */
  labels?: Record<string, string>;
}

type GrpcClientLike = {
  getConfig(project: string, branch: string, version?: number): Promise<any>;
  getItem(project: string, branch: string, group: string, key: string, version?: number): Promise<unknown | undefined>;
  listMembers(): Promise<Member[]>;
  watch(project: string, branch: string, listener: (e: any) => void, signal?: AbortSignal): void;
  close(): void;
};

/** 普通请求超时（F-SDK） */
const REQUEST_TIMEOUT_MS = 10_000;

export class ConfigClient {
  private endpoints: Endpoint[];
  private grpc: GrpcClientLike | null = null;
  private grpcReady: Promise<GrpcClientLike | null> | null = null;
  private token?: string;
  private instance?: string;
  private labels?: Record<string, string>;

  constructor(endpoints: Endpoint[], opts?: ConfigClientOptions) {
    this.endpoints = endpoints;
    this.token = opts?.token;
    this.instance = opts?.instance;
    this.labels = opts?.labels;
  }

  /** 懒加载 gRPC 客户端：仅当端点带 grpc 地址时动态 import（HTTP-only/浏览器零依赖）。
   *  F-SDK：import 失败（缺 @grpc/grpc-js 等）捕获后回落 HTTP 通道，不再产生未处理 rejection。 */
  private ensureGrpc(): Promise<GrpcClientLike | null> {
    if (this.grpc) return Promise.resolve(this.grpc);
    if (this.grpcReady) return this.grpcReady;
    const ep = this.endpoints[0];
    if (ep && typeof ep === 'object' && ep.grpc) {
      const grpcAddr = ep.grpc;
      this.grpcReady = import('./grpc.ts')
        .then((m) => {
          this.grpc = new m.GrpcConfigClient({
            grpc: grpcAddr,
            token: this.token,
            instanceId: this.instance,
            labels: this.labels,
          }) as GrpcClientLike;
          return this.grpc;
        })
        .catch((e) => {
          console.warn('gRPC 通道加载失败，回落 HTTP/SSE：', e?.message ?? e);
          this.grpcReady = null; // 允许下次重试
          return null;
        });
    } else {
      this.grpcReady = Promise.resolve(null);
    }
    return this.grpcReady;
  }

  private httpEndpoint(ep: Endpoint): string {
    if (typeof ep === 'object' && ep.http) return ep.http;
    return ep as string;
  }

  private wrapGrpcErr(e: unknown): never {
    if (e instanceof ConfigError) throw e;
    const err = e as Error & { code?: string };
    throw new ConfigError(err.code ?? 'GRPC_ERROR', err.message);
  }

  private async request<T>(path: string): Promise<T> {
    const attempts = this.endpoints.length * 2;
    for (let i = 0; i < attempts; i++) {
      const ep = this.httpEndpoint(this.endpoints[i % this.endpoints.length]);
      try {
        const headers: Record<string, string> = {};
        if (this.token) headers['Authorization'] = 'Bearer ' + this.token;
        if (this.instance) headers['X-Dsh-Instance'] = this.instance;
        if (this.labels && Object.keys(this.labels).length > 0) {
          headers['X-Dsh-Labels'] = Object.keys(this.labels)
            .sort()
            .map((k) => k + '=' + this.labels![k])
            .join(',');
        }
        // F-SDK：请求超时（默认 10s），挂死端点不再永久 pending
        const ctrl = new AbortController();
        const timer = setTimeout(() => ctrl.abort(), REQUEST_TIMEOUT_MS);
        try {
          const r = await fetch(ep + path, { headers, signal: ctrl.signal });
          if (!r.ok) throw new ConfigError('HTTP_' + r.status, 'GET ' + path + ' -> ' + r.status);
          return (await r.json()) as T;
        } finally {
          clearTimeout(timer);
        }
      } catch (e) {
        if (e instanceof ConfigError) throw e;
        await new Promise((res) => setTimeout(res, 200 * (i + 1)));
      }
    }
    throw new ConfigError('NO_ENDPOINT', 'all endpoints unreachable');
  }

  async get(project: string, branch: string, version = 0): Promise<Snapshot> {
    const g = await this.ensureGrpc();
    if (g) {
      try {
        return await g.getConfig(project, branch, version);
      } catch (e) {
        this.wrapGrpcErr(e);
      }
    }
    return this.request<Snapshot>(
      '/v1/projects/' + project + '/branches/' + branch + '/snapshot',
    );
  }

  async getItem(
    project: string,
    branch: string,
    group: string,
    key: string,
    version = 0,
  ): Promise<unknown | undefined> {
    const g = await this.ensureGrpc();
    if (g) {
      try {
        return await g.getItem(project, branch, group, key, version);
      } catch (e) {
        this.wrapGrpcErr(e);
      }
    }
    const s = await this.get(project, branch);
    return s.groups?.[group]?.[key];
  }

  /** 集群成员（端点池动态刷新；仅 gRPC 通道提供）。 */
  async listMembers(): Promise<Member[]> {
    const g = await this.ensureGrpc();
    if (g) {
      try {
        return await g.listMembers();
      } catch (e) {
        this.wrapGrpcErr(e);
      }
    }
    throw new ConfigError('NO_GRPC', 'listMembers 需要 gRPC 端点（Endpoint{grpc}）');
  }

  /**
   * 订阅 (项目, 分支) 的发布事件；断线自动以 after_version 续传重连（退避）；
   * signal.abort 停止。gRPC 通道事件含 snapshot_required 标志。
   */
  watch(
    project: string,
    branch: string,
    listener: (e: WatchEvent) => void,
    signal?: AbortSignal,
  ): void {
    const g = this.ensureGrpc();
    g.then((client) => {
      if (client) {
        client.watch(project, branch, listener, signal);
      } else {
        this.watchHttp(project, branch, listener, signal);
      }
    }).catch(() => {
      // ensureGrpc 已自捕获；此处兜底防未处理 rejection
      this.watchHttp(project, branch, listener, signal);
    });
    return;
  }

  private watchHttp(
    project: string,
    branch: string,
    listener: (e: WatchEvent) => void,
    signal?: AbortSignal,
  ): void {
    const path = '/v1/projects/' + project + '/branches/' + branch + '/watch';
    let lastVersion = 0;
    const connect = (attempt: number) => {
      if (signal?.aborted) return;
      const ctrl = new AbortController();
      const onAbort = () => ctrl.abort();
      signal?.addEventListener('abort', onAbort);
      // B1 契约：订阅/重连先做一次 snapshot 拉取，重锚版本游标
      // （灰度 publish/abort 不写 v/ 记录，版本链重放不含 → 断线期间撤回的灰度须靠快照回落）。
      this.get(project, branch)
        .then((snap) => {
          if (snap.version && snap.version > lastVersion) lastVersion = snap.version;
          const resume = lastVersion > 0 ? '?after_version=' + lastVersion : '';
          const headers: Record<string, string> = {};
          if (this.token) headers['Authorization'] = 'Bearer ' + this.token;
          return fetch(this.httpEndpoint(this.endpoints[0]) + path + resume, { signal: ctrl.signal, headers });
        })
        .then(async (r) => {
          if (!r.ok || !r.body) throw new ConfigError('HTTP_' + r.status, 'watch failed');
          const reader = r.body.getReader();
          const dec = new TextDecoder();
          let buf = '';
          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;
            buf += dec.decode(value, { stream: true });
            let nl: number;
            while ((nl = buf.indexOf('\n')) >= 0) {
              const line = buf.slice(0, nl).trim();
              buf = buf.slice(nl + 1);
              if (line.startsWith('data:')) {
                try {
                  const ev = JSON.parse(line.slice(5).trim()) as WatchEvent;
                  if (!ev.gray && ev.version <= lastVersion) continue; // F-SDK：重放/重连重复投递去重；灰度事件永不过滤
                  if (!ev.gray) lastVersion = ev.version;
                  listener(ev);
                } catch {
                  /* 忽略坏帧 */
                }
              }
            }
          }
          schedule(attempt);
        })
        .catch(() => schedule(attempt));
      const schedule = (a: number) => {
        signal?.removeEventListener('abort', onAbort);
        if (!signal?.aborted) setTimeout(() => connect(a + 1), Math.min(1000 * 2 ** a, 15000));
      };
    };
    connect(0);
  }

  close(): void {
    this.ensureGrpc()
      .then((g) => g?.close())
      .catch(() => {
        /* close 失败忽略 */
      });
  }
}
