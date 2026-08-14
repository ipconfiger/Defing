// Defing ConfigClient — TypeScript SDK（浏览器/Node fetch + SSE）。
// 端点池 failover：连接失败自动切换下一个端点（指数退避）。

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
}

export interface Snapshot {
  project: string;
  branch: string;
  version: number;
  structure_version: number;
  groups: Record<string, Record<string, unknown>>;
}

export class ConfigError extends Error {
  public code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

export class ConfigClient {
  private endpoints: string[];

  constructor(endpoints: string[]) {
    this.endpoints = endpoints;
  }

  private async request<T>(path: string): Promise<T> {
    const attempts = this.endpoints.length * 2;
    for (let i = 0; i < attempts; i++) {
      const ep = this.endpoints[i % this.endpoints.length];
      try {
        const r = await fetch(ep + path);
        if (!r.ok) throw new ConfigError('HTTP_' + r.status, 'GET ' + path + ' -> ' + r.status);
        return (await r.json()) as T;
      } catch (e) {
        if (e instanceof ConfigError) throw e;
        await new Promise((res) => setTimeout(res, 200 * (i + 1)));
      }
    }
    throw new ConfigError('NO_ENDPOINT', 'all endpoints unreachable');
  }

  async get(project: string, branch: string): Promise<Snapshot> {
    return this.request<Snapshot>(
      '/v1/projects/' + project + '/branches/' + branch + '/snapshot',
    );
  }

  async getItem(project: string, branch: string, group: string, key: string): Promise<unknown | undefined> {
    const s = await this.get(project, branch);
    return s.groups?.[group]?.[key];
  }

  /** 订阅 (项目, 分支) 的发布事件；断线自动重连（退避）；signal.abort 停止。 */
  watch(
    project: string,
    branch: string,
    listener: (e: WatchEvent) => void,
    signal?: AbortSignal,
  ): void {
    const path = '/v1/projects/' + project + '/branches/' + branch + '/watch';
    const connect = (attempt: number) => {
      if (signal?.aborted) return;
      const ctrl = new AbortController();
      const onAbort = () => ctrl.abort();
      signal?.addEventListener('abort', onAbort);
      fetch(this.endpoints[0] + path, { signal: ctrl.signal })
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
                  listener(JSON.parse(line.slice(5).trim()) as WatchEvent);
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
}
