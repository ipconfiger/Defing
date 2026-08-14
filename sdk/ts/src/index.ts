// Defing ConfigClient — TypeScript SDK（浏览器/Node）。
// 端点池 failover：连接失败自动切换下一个端点（指数退避）。
// 数据面双通道：端点可带 grpc 地址（design §3.1 Endpoint{grpc?,http?}）→ 走 gRPC（:8383）；
// 纯字符串端点 → HTTP/SSE（降级通道）。两通道 API 形状一致。

import { GrpcConfigClient } from './grpc.ts';

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
}

export interface Snapshot {
  project: string;
  branch: string;
  version: number;
  structure_version: number;
  groups: Record<string, Record<string, unknown>>;
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
}

export class ConfigClient {
  private endpoints: Endpoint[];
  private grpc: GrpcConfigClient | null = null;
  private token?: string;

  constructor(endpoints: Endpoint[], opts?: ConfigClientOptions) {
    this.endpoints = endpoints;
    this.token = opts?.token;
    // 优先首个端点的 gRPC 地址（design §3.1）
    const ep = endpoints[0];
    if (ep && typeof ep === 'object' && ep.grpc) {
      this.grpc = new GrpcConfigClient({ grpc: ep.grpc, token: opts?.token });
    }
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
        const r = await fetch(ep + path, { headers });
        if (!r.ok) throw new ConfigError('HTTP_' + r.status, 'GET ' + path + ' -> ' + r.status);
        return (await r.json()) as T;
      } catch (e) {
        if (e instanceof ConfigError) throw e;
        await new Promise((res) => setTimeout(res, 200 * (i + 1)));
      }
    }
    throw new ConfigError('NO_ENDPOINT', 'all endpoints unreachable');
  }

  async get(project: string, branch: string, version = 0): Promise<Snapshot> {
    if (this.grpc) {
      try {
        return await this.grpc.getConfig(project, branch, version);
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
    if (this.grpc) {
      try {
        return await this.grpc.getItem(project, branch, group, key, version);
      } catch (e) {
        this.wrapGrpcErr(e);
      }
    }
    const s = await this.get(project, branch);
    return s.groups?.[group]?.[key];
  }

  /** 集群成员（端点池动态刷新；仅 gRPC 通道提供）。 */
  async listMembers(): Promise<Member[]> {
    if (this.grpc) {
      try {
        return await this.grpc.listMembers();
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
    if (this.grpc) {
      this.grpc.watch(project, branch, listener, signal);
      return;
    }
    const path = '/v1/projects/' + project + '/branches/' + branch + '/watch';
    let lastVersion = 0;
    const connect = (attempt: number) => {
      if (signal?.aborted) return;
      const ctrl = new AbortController();
      const onAbort = () => ctrl.abort();
      signal?.addEventListener('abort', onAbort);
      // 断线重连带 after_version 续传（design §6.2）
      const resume = lastVersion > 0 ? '?after_version=' + lastVersion : '';
      fetch(this.httpEndpoint(this.endpoints[0]) + path + resume, { signal: ctrl.signal })
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
                  if (ev.version > lastVersion) lastVersion = ev.version;
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
    this.grpc?.close();
  }
}
