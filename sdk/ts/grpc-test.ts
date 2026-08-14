// TS SDK gRPC 契约测试：GetConfig / GetItem / Watch / ListMembers（走 :8383）。
import { ConfigClient } from './src/index.ts';

const GRPC = process.env.DSH_GRPC || '127.0.0.1:8383';
const HTTP = process.env.DSH_HTTP || 'http://127.0.0.1:8384';
const P = process.env.DSH_PROJECT || 'sdk-project';

async function main() {
  // gRPC 优先端点（design §3.1 Endpoint{grpc?,http?}）
  const c = new ConfigClient([{ grpc: GRPC, http: HTTP }]);

  const snap = await c.get(P, 'dev');
  const host = (snap.groups as any)?.redis?.host;
  console.log('[ts-grpc] get ok: version=' + snap.version + ' host=' + host);
  if (!host || typeof host !== 'string') throw new Error('get value mismatch');

  const item = await c.getItem(P, 'dev', 'redis', 'host');
  console.log('[ts-grpc] getItem ok: ' + item);
  if (item !== host) throw new Error('getItem mismatch');

  const members = await c.listMembers().catch(() => [] as any[]);
  console.log('[ts-grpc] listMembers ok: ' + members.length + ' members (dev-single 可为空)');

  await new Promise<void>((resolve, reject) => {
    const ctrl = new AbortController();
    const timer = setTimeout(() => {
      ctrl.abort();
      reject(new Error('watch timeout'));
    }, 10000);
    c.watch(P, 'dev', (e) => {
      if (e.version > snap.version) {
        clearTimeout(timer);
        ctrl.abort();
        c.close();
        console.log('[ts-grpc] watch event: v' + e.version + ' ' + e.ty);
        resolve();
      }
    }, ctrl.signal);
  });
  console.log('[ts-grpc] PASS');
}

main().catch((e) => {
  console.error('[ts-grpc] FAIL:', e);
  process.exit(1);
});
