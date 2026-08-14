// TS SDK 契约测试：get + watch（发布事件）。
import { ConfigClient } from './src/index.ts';

const ENDPOINTS = (process.env.DSH_ENDPOINTS || 'http://127.0.0.1:8384').split(',');
const P = process.env.DSH_PROJECT || 'sdk-project';

async function main() {
  const c = new ConfigClient(ENDPOINTS);
  const snap = await c.get(P, 'dev');
  const host = (snap.groups as any)?.redis?.host;
  console.log('[ts] get ok: version=' + snap.version + ' host=' + host);
  if (!host || typeof host !== 'string') throw new Error('get value mismatch: ' + JSON.stringify(snap.groups));

  const ctrl = new AbortController();
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('watch timeout')), 10000);
    c.watch(P, 'dev', (e) => {
      if (e.version > snap.version) {
        clearTimeout(timer);
        ctrl.abort();
        console.log('[ts] watch event: v' + e.version + ' ' + e.ty + ' changes=' + e.changes.length);
        resolve();
      }
    }, ctrl.signal);
  });
  console.log('[ts] PASS');
}

main().catch((e) => {
  console.error('[ts] FAIL:', e);
  process.exit(1);
});
