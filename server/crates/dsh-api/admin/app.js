/* Defing 配置中心 Admin UI —— 外置脚本（D-CSP：移除 inline script 与 unsafe-inline） */
'use strict';

let TOKEN = localStorage.getItem('dsh_admin_token') || '';
let curProject = '', curBranch = '', curVersion = 0, watchES = null;

const $ = (id) => document.getElementById(id);
function auth() {
  return TOKEN ? { 'Authorization': 'Bearer ' + TOKEN, 'Content-Type': 'application/json' } : { 'Content-Type': 'application/json' };
}
function msg(text, ok) {
  const m = $('msg');
  m.className = 'msg ' + (ok ? 'ok' : 'err');
  m.textContent = text;
  setTimeout(() => { m.className = 'msg'; }, 5000);
}
async function j(method, url, body) {
  const r = await fetch(url, { method, headers: auth(), body: body ? JSON.stringify(body) : undefined });
  if (r.status === 401 && !url.endsWith('/login')) {
    // 会话过期 → 清 token 回登录页（不再停留在"已登录"假象）
    TOKEN = ''; localStorage.removeItem('dsh_admin_token');
    $('view-main').classList.add('hidden');
    $('view-login').classList.remove('hidden');
  }
  if (!r.ok) {
    let detail = '';
    try { detail = (await r.json()).message || ''; } catch (_) {}
    throw new Error('HTTP ' + r.status + ' ' + detail);
  }
  if (r.status === 204) return null;
  return r.json();
}
const rid = () => 'ui-' + Date.now() + '-' + Math.floor(Math.random() * 1e6);

// ---------- 事件委托（D-CSP：onclick/onchange 属性全部外置为 data-act） ----------
document.addEventListener('click', (e) => {
  const el = e.target.closest('[data-act]');
  if (!el) return;
  const fn = window[el.dataset.act];
  if (typeof fn === 'function') fn.call(el, el);
});
document.addEventListener('change', (e) => {
  const el = e.target.closest('[data-act]');
  if (!el) return;
  const fn = window[el.dataset.act];
  if (typeof fn === 'function') fn.call(el, el);
});

// ---------- modal ----------
let modalCb = null;
function askModal(title, placeholder, cb) {
  $('modal-title').textContent = title;
  $('modal-input').value = '';
  $('modal-input').placeholder = placeholder || '';
  modalCb = cb;
  $('modal').classList.remove('hidden');
  $('modal-input').focus();
}
function closeModal() { $('modal').classList.add('hidden'); modalCb = null; }
$('modal-ok').onclick = () => { const v = $('modal-input').value; const cb = modalCb; closeModal(); if (cb) cb(v); };
$('modal-cancel').onclick = closeModal;

// ---------- 登录 ----------
function doLogin() {
  j('POST', '/api/v1/login', { password: $('login-pw').value })
    .then((r) => { TOKEN = r.token; localStorage.setItem('dsh_admin_token', TOKEN); enterMain(); msg('登录成功', true); })
    .catch((e) => msg(e.message, false));
}
function doLogout() {
  j('POST', '/api/v1/logout', {}).catch(() => {});
  TOKEN = ''; localStorage.removeItem('dsh_admin_token'); location.reload();
}
function enterMain() {
  $('view-login').classList.add('hidden');
  $('view-main').classList.remove('hidden');
  $('btn-logout').classList.remove('hidden');
  loadProjects(); loadShared(); loadAudit(); loadCluster();
}
function showTab(el) {
  const t = el.dataset.arg;
  document.querySelectorAll('.tabs button').forEach((b) => b.classList.toggle('active', b.dataset.tab === t));
  $('tab-projects').classList.toggle('hidden', t !== 'projects');
  $('tab-shared').classList.toggle('hidden', t !== 'shared');
  $('tab-audit').classList.toggle('hidden', t !== 'audit');
  $('tab-cluster').classList.toggle('hidden', t !== 'cluster');
  if (t === 'audit') loadAudit();
  if (t === 'shared') loadShared();
  if (t === 'cluster') loadCluster();
}

// ---------- 项目 ----------
function loadProjects() {
  j('GET', '/api/v1/projects').then((ps) => {
    const sel = $('sel-proj');
    sel.innerHTML = '<option value="">选择项目…</option>' + ps.map((p) => `<option value="${p.id}">${p.name}</option>`).join('');
    if (curProject) sel.value = curProject;
    if (curProject) loadProject();
  }).catch((e) => msg(e.message, false));
}
function createProject() {
  const n = $('new-proj').value.trim();
  if (!n) return msg('请输入项目名', false);
  j('POST', '/api/v1/projects', { name: n }).then(() => { $('new-proj').value = ''; loadProjects(); msg('项目已创建', true); }).catch((e) => msg(e.message, false));
}
function deleteProject() {
  if (!curProject) return;
  askModal('确认删除项目 ' + curProject + '？（force=true）', '', () => {
    j('DELETE', '/api/v1/projects/' + curProject + '?force=true').then(() => { curProject = ''; loadProjects(); msg('项目已删除', true); }).catch((e) => msg(e.message, false));
  });
}
function loadProject() {
  curProject = $('sel-proj').value;
  if (!curProject) { $('proj-detail').classList.add('hidden'); return; }
  $('proj-detail').classList.remove('hidden');
  $('proj-title').textContent = '项目：' + curProject;
  Promise.all([j('GET', `/api/v1/projects/${curProject}/branches`), j('GET', `/api/v1/projects/${curProject}/structure-draft`)])
    .then(([bs]) => {
      $('sel-branch').innerHTML = bs.map((b) => `<option value="${b.name}">${b.name} (v${b.active_version})</option>`).join('');
      $('diff-a').innerHTML = bs.map((b) => `<option>${b.name}</option>`).join('');
      $('diff-b').innerHTML = bs.map((b) => `<option>${b.name}</option>`).join('');
      $('promote-from').innerHTML = bs.map((b) => `<option>${b.name}</option>`).join('');
      $('promote-to').innerHTML = bs.map((b) => `<option>${b.name}</option>`).join('');
      // 保持当前分支选择（发布/回滚/提升后不再跳回第一个分支；仅当分支被删时回落）
      if (bs.length) {
        curBranch = bs.some((b) => b.name === curBranch) ? curBranch : bs[0].name;
        $('sel-branch').value = curBranch; loadBranch();
      }
    }).catch((e) => msg(e.message, false));
}
function createBranch() {
  const n = $('new-branch').value.trim();
  if (!n || !curProject) return msg('请输入分支名', false);
  j('POST', `/api/v1/projects/${curProject}/branches`, { name: n }).then(() => { $('new-branch').value = ''; loadProject(); msg('分支已创建', true); }).catch((e) => msg(e.message, false));
}
function deleteBranch() {
  if (!curBranch) return;
  askModal('确认删除分支 ' + curBranch + '？', '', () => {
    j('DELETE', `/api/v1/projects/${curProject}/branches/${curBranch}`).then(() => { loadProject(); msg('分支已删除', true); }).catch((e) => msg(e.message, false));
  });
}

// ---------- 结构 ----------
function loadStructDraft() {
  j('GET', `/api/v1/projects/${curProject}/structure-draft`).then((d) => { $('struct-draft').value = JSON.stringify(d, null, 2); }).catch((e) => msg(e.message, false));
}
function saveStructDraft() {
  let d; try { d = JSON.parse($('struct-draft').value); } catch (e) { return msg('结构 JSON 非法: ' + e.message, false); }
  j('PUT', `/api/v1/projects/${curProject}/structure-draft`, d).then(() => msg('结构草稿已保存', true)).catch((e) => msg(e.message, false));
}
function publishStruct() {
  askModal('发布结构备注', '备注（可选）', (comment) => {
    j('POST', `/api/v1/projects/${curProject}/structure-draft/publish`, { comment, request_id: rid() })
      .then((r) => { msg('结构已发布：' + JSON.stringify(r), true); loadProject(); }).catch((e) => msg(e.message, false));
  });
}

// ---------- 分支草稿 ----------
function loadBranch() {
  curBranch = $('sel-branch').value;
  if (!curBranch) return;
  $('branch-title').textContent = '分支草稿编辑：' + curBranch;
  Promise.all([j('GET', `/api/v1/projects/${curProject}/branches/${curBranch}`), loadVersions()])
    .then(([b]) => { curVersion = b.active_version || 0; renderDraftEditor(b); })
    .catch((e) => msg(e.message, false));
}
function renderDraftEditor(b) {
  const box = $('draft-editor');
  const groups = Object.keys(b.draft || {});
  let html = '';
  if (!groups.length) html = '<span class="muted">当前分支无草稿项（先改结构发布，或直接添加下方值）</span><br>';
  for (const g of groups) {
    html += `<b>${esc(g)}</b><table><thead><tr><th>key</th><th>值</th><th>删除</th></tr></thead><tbody>`;
    for (const [k, dv] of Object.entries(b.draft[g] || {})) {
      const v = dv.value; const type = v && v.type ? v.type : typeof v;
      // 按类型渲染——修复 int/float/json/array 显示 [object Object] 与 bool 恒不勾选
      let input;
      if (type === 'bool') {
        input = `<input type="checkbox" data-g="${esc(g)}" data-k="${esc(k)}" ${v.bool_value === true ? 'checked' : ''}>`;
      } else if (type === 'int' || type === 'float') {
        input = `<input type="number" step="${type === 'float' ? 'any' : '1'}" data-g="${esc(g)}" data-k="${esc(k)}" data-ty="${esc(type)}" value="${esc(type === 'int' ? (v.int_value ?? '') : (v.float_value ?? ''))}">`;
      } else if (type === 'json') {
        input = `<textarea data-g="${esc(g)}" data-k="${esc(k)}" data-ty="json" rows="2">${esc(v.json_value ?? '')}</textarea>`;
      } else if (type === 'array') {
        input = `<input data-g="${esc(g)}" data-k="${esc(k)}" data-ty="array" value="${esc((v.list_value || []).join(', '))}">`;
      } else if (type === 'secret') {
        // secret 值密文不回显；留空 = 不修改，输入 = 提交明文由服务端加密
        input = `<input type="password" data-g="${esc(g)}" data-k="${esc(k)}" data-ty="secret" placeholder="已加密（留空不改，输入以修改）">`;
      } else {
        input = `<input data-g="${esc(g)}" data-k="${esc(k)}" data-ty="${esc(type)}" value="${esc(v.str_value ?? '')}">`;
      }
      // D-CSP：onclick 改 data-act（g/k 经 dataset 传递，无 JS 字符串注入面）
      html += `<tr><td class="mono">${esc(k)}</td><td>${input}</td><td><button class="danger" data-act="delDraftItem" data-g="${esc(g)}" data-k="${esc(k)}">删</button></td></tr>`;
    }
    html += '</tbody></table>';
  }
  html += `<div class="row" style="margin-top:10px">
    <input id="new-item-group" placeholder="组" style="width:100px">
    <input id="new-item-key" placeholder="key" style="width:140px">
    <select id="new-item-type"><option value="string">string</option><option value="int">int</option><option value="float">float</option><option value="bool">bool</option><option value="json">json</option><option value="array">array</option><option value="secret">secret</option></select>
    <input id="new-item-val" placeholder="值" style="width:200px">
    <button data-act="addDraftItem">添加</button>
  </div>`;
  html += `<div class="row"><button class="primary" data-act="saveDraft">保存草稿</button>
    <button data-act="doPublish">发布版本</button>
    <span class="muted">发布后 SDK watch 收到事件</span></div>`;
  box.innerHTML = html;
}
function esc(s) { return String(s ?? '').replace(/&/g,'&amp;').replace(/"/g,'&quot;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/'/g,'&#39;'); }
function addDraftItem() {
  const g = $('new-item-group').value.trim(), k = $('new-item-key').value.trim(), ty = $('new-item-type').value, raw = $('new-item-val').value;
  if (!g || !k) return msg('组和 key 必填', false);
  let value;
  try { value = buildValue(ty, raw); } catch (e) { return msg(e.message, false); }
  j('PUT', `/api/v1/projects/${curProject}/branches/${curBranch}/draft`, { updates: [{ group: g, key: k, value }], deletes: [] })
    .then(() => { loadBranch(); msg('草稿项已添加', true); }).catch((e) => msg(e.message, false));
}
function delDraftItem(el) {
  const g = el.dataset.g, k = el.dataset.k;
  j('PUT', `/api/v1/projects/${curProject}/branches/${curBranch}/draft`, { updates: [], deletes: [g + '/' + k] })
    .then(() => { loadBranch(); msg('草稿项已删除', true); }).catch((e) => msg(e.message, false));
}
function buildValue(ty, raw) {
  // 非法数值显式报错，不再静默置 0（数据破坏）
  switch (ty) {
    case 'int': { const n = parseInt(raw, 10); if (Number.isNaN(n)) throw new Error('int 值非法: ' + raw); return { type: 'int', int_value: n }; }
    case 'float': { const n = parseFloat(raw); if (Number.isNaN(n)) throw new Error('float 值非法: ' + raw); return { type: 'float', float_value: n }; }
    case 'bool': return { type: 'bool', bool_value: raw === 'true' || raw === 'on' };
    case 'json': return { type: 'json', json_value: raw };
    case 'array': return { type: 'array', list_value: raw.split(',').map((s) => s.trim()).filter(Boolean) };
    case 'secret': return { type: 'string', str_value: raw };
    default: return { type: 'string', str_value: raw };
  }
}
function saveDraft() {
  const updates = [];
  document.querySelectorAll('#draft-editor input[data-g][data-k]:not([type=checkbox]):not([type=password])').forEach((inp) => {
    const ty = inp.dataset.ty || 'string';
    let value;
    try { value = buildValue(ty, inp.value); } catch (e) { return msg(e.message, false); }
    updates.push({ group: inp.dataset.g, key: inp.dataset.k, value });
  });
  // secret 密码框：留空 = 不修改（跳过，服务端保留原密文）
  document.querySelectorAll('#draft-editor input[type=password][data-g][data-k]').forEach((inp) => {
    if (!inp.value) return;
    updates.push({ group: inp.dataset.g, key: inp.dataset.k, value: { type: 'string', str_value: inp.value } });
  });
  document.querySelectorAll('#draft-editor input[type=checkbox][data-g][data-k]').forEach((inp) => {
    updates.push({ group: inp.dataset.g, key: inp.dataset.k, value: { type: 'bool', bool_value: inp.checked } });
  });
  j('PUT', `/api/v1/projects/${curProject}/branches/${curBranch}/draft`, { updates, deletes: [] })
    .then(() => msg('草稿已保存', true)).catch((e) => msg(e.message, false));
}
function doPublish() {
  askModal('发布版本备注', '备注（可选）', (comment) => {
    j('POST', `/api/v1/projects/${curProject}/branches/${curBranch}/publish`, { comment, request_id: rid() })
      .then((r) => { msg('已发布 version=' + r.version, true); loadProject(); }).catch((e) => msg(e.message, false));
  });
}

// ---------- 活动配置查看（P3-⑦ UI 功能面：reveal 入口） ----------
function viewConfig() {
  if (!curProject || !curBranch) return msg('请先选择项目/分支', false);
  const reveal = $('cfg-reveal') ? $('cfg-reveal').checked : false;
  // reveal=true 走管理面（会话鉴权 + 审计）；默认数据面（掩码）
  const url = reveal
    ? `/api/v1/projects/${curProject}/branches/${curBranch}/config?reveal=true`
    : `/v1/projects/${curProject}/branches/${curBranch}/config?format=json`;
  j('GET', url).then((d) => {
    $('cfg-out').classList.remove('hidden');
    $('cfg-out').textContent = typeof d === 'string' ? d : JSON.stringify(d, null, 2);
    msg(reveal ? '已显示明文（已审计）' : '默认掩码（secret 显示 ***）', true);
  }).catch((e) => msg(e.message, false));
}

// ---------- 版本历史 / 回滚 ----------
function loadVersions() {
  if (!curProject) return Promise.resolve();
  return j('GET', `/api/v1/projects/${curProject}/branches/${curBranch}/versions`).then((vs) => {
    const tb = document.querySelector('#versions tbody');
    if (!tb) return;
    tb.innerHTML = (vs || []).map((v) => `<tr>
      <td>v${v.no}</td><td>${v.rollback_of ? '回滚←v' + v.rollback_of : (v.kind || 'full')}</td>
      <td>${esc(v.comment || '')}</td><td>sv${v.structure_version}</td><td class="muted">${new Date(v.created_at).toLocaleString()}</td>
      <td><button data-act="doRollback" data-ver="${v.no}">回滚到此处</button></td></tr>`).join('');
  });
}
function doRollback(el) {
  const toVersion = Number(el.dataset.ver);
  askModal('回滚备注（回滚 = 以旧内容创建新版本，历史不可变 I6）', '', (comment) => {
    j('POST', `/api/v1/projects/${curProject}/branches/${curBranch}/rollback`, { to_version: toVersion, comment, request_id: rid() })
      .then((r) => { msg('已回滚，新版本 v' + r.new_version, true); loadProject(); }).catch((e) => msg(e.message, false));
  });
}

// ---------- 对比 / 提升 ----------
function showDiff() {
  const a = $('diff-a').value, b = $('diff-b').value;
  if (!a || !b) return msg('请选择对比分支', false);
  j('GET', `/api/v1/projects/${curProject}/diff?branch_a=${a}&branch_b=${b}`).then((d) => {
    let html = '<table><thead><tr><th>key</th><th>' + esc(a) + '</th><th>' + esc(b) + '</th></tr></thead><tbody>';
    for (const x of d.diffs || []) html += `<tr><td class="mono">${esc(x.group)}/${esc(x.key)}</td><td>${esc(JSON.stringify(x.branch_a))}</td><td>${esc(JSON.stringify(x.branch_b))}</td></tr>`;
    for (const m of d.missing || []) html += `<tr><td class="mono">${esc(m)}</td><td colspan="2" class="muted">仅一侧有值</td></tr>`;
    html += '</tbody></table>';
    if (!(d.diffs || []).length && !(d.missing || []).length) html = '<span class="muted">两个分支完全一致</span>';
    $('diff-out').innerHTML = html;
  }).catch((e) => msg(e.message, false));
}
function doPromote() {
  const from = $('promote-from').value, to = $('promote-to').value;
  if (!from || !to) return msg('请选择提升源/目标', false);
  j('POST', `/api/v1/projects/${curProject}/promote`, { from, to, force: $('promote-force').checked })
    .then((r) => { msg('提升完成：applied=' + r.applied.length + ' skipped=' + r.skipped.length + ' missing=' + r.missing_from.length, true); loadBranch(); })
    .catch((e) => msg(e.message, false));
}

// ---------- watch（P3-⑦：after_version 续传 + 事件面板截断） ----------
function toggleWatch() {
  if (watchES) { watchES.close(); watchES = null; $('events').classList.add('hidden'); $('btn-watch').textContent = '订阅变更'; return; }
  if (!curProject || !curBranch) return;
  $('events').classList.remove('hidden');
  $('events').textContent = '订阅 ' + curProject + '/' + curBranch + '（after_version=' + curVersion + '）...\n';
  // 断线重连带 after_version 续传（不再丢事件）
  watchES = new EventSource(`/v1/projects/${curProject}/branches/${curBranch}/watch?after_version=${curVersion}`);
  watchES.onmessage = (ev) => {
    let txt = $('events').textContent + ev.data + '\n';
    // 截断保留最近 200 条（内存增长防护）
    const lines = txt.split('\n');
    if (lines.length > 200) txt = lines.slice(lines.length - 200).join('\n');
    $('events').textContent = txt;
    $('events').scrollTop = $('events').scrollHeight;
    try { curVersion = Math.max(curVersion, JSON.parse(ev.data).version || 0); } catch (_) {}
  };
  watchES.onerror = () => { $('events').textContent += '（断线重连…）\n'; };
  $('btn-watch').textContent = '停止订阅';
}

// ---------- 共享库 ----------
function loadShared() {
  Promise.all([j('GET', '/api/v1/shared').catch(() => []), j('GET', '/api/v1/shared-draft').catch(() => [])]).then(([pub, draft]) => {
    const tb = document.querySelector('#shared tbody');
    if (!tb) return;
    const rows = (draft.map((s) => ({ ...s, __draft: true }))).concat(pub.map((s) => ({ ...s, __draft: false })));
    tb.innerHTML = rows.map((s) => `<tr>
      <td class="mono">${esc(s.group)}</td><td class="mono">${esc(s.key)}</td><td>${esc(s.type)}</td>
      <td>${s.__draft ? '<span class="badge">草稿</span>' : 'v' + s.version}</td>
      <td class="mono">${esc(JSON.stringify(s.value))}</td><td>${s.secret ? '<span class="badge">secret</span>' : ''}</td></tr>`).join('');
  }).catch(() => {});
}
function saveShared() {
  let value; try { value = JSON.parse($('sh-value').value); } catch (e) { return msg('值 JSON 非法: ' + e.message, false); }
  const body = { group: $('sh-group').value.trim(), key: $('sh-key').value.trim(), type: $('sh-type').value, secret: $('sh-secret').checked, required: $('sh-required').checked, value };
  if (!body.group || !body.key) return msg('组和 key 必填', false);
  j('POST', '/api/v1/shared', body).then(() => { loadShared(); msg('共享草稿已保存', true); }).catch((e) => msg(e.message, false));
}
function publishShared() {
  askModal('发布共享备注（auto 级联引用项目）', '', (comment) => {
    j('POST', '/api/v1/shared/publish', { comment, request_id: rid() }).then((r) => {
      msg('共享已发布，受影响分支：' + (r.affected || []).length, true); loadShared();
    }).catch((e) => msg(e.message, false));
  });
}
function bindRef() {
  const body = {
    project: $('ref-project').value.trim(), group: $('ref-group').value.trim(),
    item_key: $('ref-item').value.trim() || null,
    shared_group: $('ref-sg').value.trim(), shared_key: $('ref-sk').value.trim(),
  };
  if (!body.project || !body.group || !body.shared_group || !body.shared_key) return msg('引用字段不完整', false);
  j('POST', '/api/v1/shared/refs', body).then(() => msg('引用已绑定', true)).catch((e) => msg(e.message, false));
}

// ---------- 审计 ----------
function loadAudit() {
  const f = $('audit-filter').value.trim();
  j('GET', '/api/v1/audit?limit=200' + (f ? '&action=' + f : '')).then((es) => {
    const tb = document.querySelector('#audit tbody');
    if (!tb) return;
    tb.innerHTML = es.map((e) => `<tr>
      <td>${e.seq}</td><td class="muted">${new Date(e.ts).toLocaleString()}</td><td>${esc(e.action)}</td>
      <td class="mono">${esc(e.project || '')}${e.branch ? '/' + esc(e.branch) : ''}</td><td>${esc(e.version ?? '')}</td>
      <td class="mono">${esc(e.request_id || '')}</td><td class="mono">${esc(JSON.stringify(e.detail || {}))}</td></tr>`).join('');
  }).catch((e) => msg(e.message, false));
}

// ---------- 集群管理（P3-⑦ UI 功能面：集群入口；admin-only，PA 403 显示空） ----------
function loadCluster() {
  j('GET', '/api/v1/cluster/members').then((m) => {
    const box = $('cluster-out');
    if (!box) return;
    const members = m.members || [];
    let html = `<div class="muted">本节点 ${m.node_id ?? 'dev-single'} · 状态 ${m.state} · 当前 leader ${m.current_leader ?? '—'}</div>`;
    if (members.length) {
      html += '<table><thead><tr><th>node_id</th><th>http</th><th>raft</th><th>gRPC</th><th>角色</th><th></th></tr></thead><tbody>';
      html += members.map((x) => `<tr>
        <td class="mono">${esc(x.node_id)}</td><td class="mono">${esc(x.http_addr || '')}</td><td class="mono">${esc(x.raft_addr || '')}</td><td class="mono">${esc(x.grpc_addr || '')}</td>
        <td>${x.is_voter ? '<span class="badge v">voter</span>' : '<span class="badge">learner</span>'}${x.is_leader ? ' <span class="badge">leader</span>' : ''}</td>
        <td>${x.is_voter
          ? `<button data-act="removeNode" data-node="${esc(x.node_id)}" data-http="${esc(x.http_addr || '')}" data-raft="${esc(x.raft_addr || '')}">移除</button>`
          : `<button class="primary" data-act="promoteNode" data-node="${esc(x.node_id)}" data-http="${esc(x.http_addr || '')}" data-raft="${esc(x.raft_addr || '')}">提升</button>`}</td>
      </tr>`).join('');
      html += '</tbody></table>';
    } else {
      html += '<div class="muted">（dev-single 无集群成员）</div>';
    }
    box.innerHTML = html;
  }).catch((e) => {
    const box = $('cluster-out');
    if (!box) return;
    // dev-single 无 /api/v1/cluster/members 路由（404）→ 明确提示；PA 无权限 → 403
    box.innerHTML = e.message.includes('404')
      ? '<span class="muted">（dev-single 模式无集群管理；集群模式才有成员/提升/移除）</span>'
      : '<span class="muted">' + esc(e.message) + '（项目管理员无集群权限）</span>';
  });
}
function promoteNode(el) {
  askModal('确认提升 node ' + el.dataset.node + ' 为 voter？', '', () => {
    j('POST', '/api/v1/cluster/promote', {
      node_id: Number(el.dataset.node),
      http_addr: el.dataset.http, raft_addr: el.dataset.raft,
    }).then(() => { msg('已提升', true); loadCluster(); }).catch((e) => msg(e.message, false));
  });
}
function removeNode(el) {
  askModal('确认移除 node ' + el.dataset.node + '？', '', () => {
    j('POST', '/api/v1/cluster/remove', { node_id: Number(el.dataset.node) })
      .then(() => { msg('已移除', true); loadCluster(); }).catch((e) => msg(e.message, false));
  });
}

// ---------- 初始化 ----------
if (TOKEN) { enterMain(); } else { $('view-login').classList.remove('hidden'); }
$('btn-login').onclick = () => { $('view-login').classList.remove('hidden'); $('view-main').classList.add('hidden'); };
$('btn-logout').onclick = doLogout;
$('login-pw').addEventListener('keydown', (ev) => { if (ev.key === 'Enter') doLogin(); });
setInterval(() => { if (TOKEN) j('POST', '/api/v1/heartbeat', {}).catch(() => {}); }, 300000);
