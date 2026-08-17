'use strict';
/* ============================================================
   Defing 配置中心 Admin UI —— 外置脚本（D-CSP：无 inline script / onclick）
   - 事件经 data-act 委托（click / change），动作注册于 actions 表
   - 所有服务端/用户数据插入 DOM 前经 esc() 转义或走 textContent
   - API 端点 / 请求响应形状 / Bearer 鉴权与旧版完全一致
   ============================================================ */

/* ---------------- 基础工具 ---------------- */
const $ = (id) => document.getElementById(id);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

function esc(s) {
  return String(s ?? '')
    .replace(/&/g, '&amp;').replace(/"/g, '&quot;')
    .replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/'/g, '&#39;');
}
const rid = () => 'ui-' + Date.now() + '-' + Math.floor(Math.random() * 1e6);
const fmtTime = (ms) => (ms ? new Date(ms).toLocaleString('zh-CN', { hour12: false }) : '—');
const skeleton = (n) => Array.from({ length: n }, () => '<div class="skel"></div>').join('');

/* ---------------- 状态 ---------------- */
const LS_TOKEN = 'dsh_admin_token', LS_ROLE = 'dsh_admin_role', LS_PROJ = 'dsh_admin_project', LS_THEME = 'dsh_theme';
const S = {
  token: '', role: '', roleProject: '',
  view: 'config', pane: 'draft',
  projects: [], project: '', branches: [], branch: '',
  version: 0, structV: 0, draftRev: 0, gray: null,
  watchES: null,
  // 未保存编辑保护：结构 textarea / 灰度规则有用户输入时，后台刷新不覆盖
  structDirty: false, structProj: '',
  grayDirty: false, grayBranch: '',
};

/* ---------------- 请求层 ---------------- */
function authHeaders() {
  const h = { 'Content-Type': 'application/json' };
  if (S.token) h.Authorization = 'Bearer ' + S.token;
  return h;
}

async function j(method, url, body) {
  const r = await fetch(url, { method, headers: authHeaders(), body: body === undefined ? undefined : JSON.stringify(body) });
  if (r.status === 401 && !url.endsWith('/login')) sessionExpired();
  if (!r.ok) {
    let detail = '';
    try { detail = (await r.json()).message || ''; } catch (_) { /* 非 JSON 错误体 */ }
    const e = new Error('HTTP ' + r.status + (detail ? ' ' + detail : ''));
    e.status = r.status;
    e.expired = r.status === 401 && !url.endsWith('/login');
    throw e;
  }
  if (r.status === 204) return null;
  return r.json();
}

async function jtext(url) {
  const r = await fetch(url, { headers: authHeaders() });
  if (r.status === 401) sessionExpired();
  if (!r.ok) {
    let detail = '';
    try { detail = (await r.json()).message || ''; } catch (_) { /* 非 JSON 错误体 */ }
    const e = new Error('HTTP ' + r.status + (detail ? ' ' + detail : ''));
    e.expired = r.status === 401;
    throw e;
  }
  return r.text();
}

function sessionExpired() {
  if (!S.token) return;
  S.token = '';
  localStorage.removeItem(LS_TOKEN); localStorage.removeItem(LS_ROLE); localStorage.removeItem(LS_PROJ);
  stopWatch();
  $('app').classList.add('hidden');
  $('login-view').classList.remove('hidden');
  toast('会话已过期，请重新登录', 'err');
}

/* 请求进行中禁用按钮并显示加载态 */
async function withBusy(el, fn) {
  if (el && el.setAttribute) { el.setAttribute('data-busy', ''); el.disabled = true; }
  try { return await fn(); }
  finally { if (el && el.removeAttribute) { el.removeAttribute('data-busy'); el.disabled = false; } }
}

/* ---------------- 通知 ---------------- */
function toast(text, kind = 'ok', ms) {
  const box = document.createElement('div');
  box.className = 'toast ' + kind;
  const icon = kind === 'err' ? 'i-alert' : kind === 'warn' ? 'i-info' : 'i-check';
  box.innerHTML =
    '<svg class="ic t-ic"><use href="#' + icon + '"/></svg>' +
    '<div class="toast-text"></div>' +
    '<button type="button" class="toast-x" title="关闭" aria-label="关闭"><svg class="ic ic-xs"><use href="#i-x"/></svg></button>';
  box.querySelector('.toast-text').textContent = text;
  $('toasts').appendChild(box);
  let gone = false;
  const kill = () => { if (gone) return; gone = true; box.classList.add('out'); setTimeout(() => box.remove(), 160); };
  box.querySelector('.toast-x').addEventListener('click', kill);
  setTimeout(kill, ms || (kind === 'err' ? 6000 : 4000));
}

/* ---------------- 表单内联错误 ---------------- */
function showErr(id, text) {
  const el = $(id);
  if (!el) return toast(text, 'err');
  el.innerHTML = '<svg class="ic"><use href="#i-alert"/></svg><span></span>';
  el.querySelector('span').textContent = text;
  el.classList.remove('hidden');
}
function hideErr(id) { const el = $(id); if (el) el.classList.add('hidden'); }

/* ---------------- 主题 ---------------- */
function applyTheme(t) {
  document.documentElement.dataset.theme = t;
  $('btn-theme-moon').classList.toggle('hidden', t !== 'light');
  $('btn-theme-sun').classList.toggle('hidden', t !== 'dark');
}
function initTheme() {
  const mql = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;
  applyTheme(localStorage.getItem(LS_THEME) || (mql && mql.matches ? 'dark' : 'light'));
}

/* ---------------- 弹窗（确认 / 输入） ---------------- */
let modalCb = null;
function openModal(o) {
  $('modal-title').textContent = o.title || '确认';
  const msg = $('modal-msg');
  if (o.message) { msg.textContent = o.message; msg.classList.remove('hidden'); }
  else msg.classList.add('hidden');
  const field = $('modal-field'), input = $('modal-input'), label = $('modal-label');
  if (o.input) {
    field.classList.remove('hidden');
    label.textContent = o.label || '';
    input.placeholder = o.placeholder || '';
    input.value = o.value || '';
    input.type = o.inputType || 'text';
  } else field.classList.add('hidden');
  const ok = $('modal-ok');
  ok.textContent = o.okText || '确定';
  ok.className = 'btn ' + (o.danger ? 'danger' : 'primary');
  modalCb = o.onOk || null;
  $('modal-overlay').classList.remove('hidden');
  if (o.input) input.focus(); else ok.focus();
}
function closeModal() { $('modal-overlay').classList.add('hidden'); modalCb = null; }

/* ============================================================
   动作表（data-act 委托）
   ============================================================ */
const actions = {};

/* ---------- 视图 / 会话 ---------- */
actions.switchView = function (el) {
  const v = el.dataset.nav;
  S.view = v;
  $$('.nav-item').forEach((b) => b.classList.toggle('active', b.dataset.nav === v));
  $$('.view').forEach((s) => s.classList.toggle('hidden', s.id !== 'view-' + v));
  if (v === 'shared') loadShared();
  if (v === 'audit') loadAudit();
  if (v === 'cluster') loadCluster();
};
actions.switchPane = function (el) {
  S.pane = el.dataset.pane;
  $$('#pane-seg button').forEach((b) => b.classList.toggle('active', b.dataset.pane === S.pane));
  $$('.pane').forEach((p) => p.classList.toggle('hidden', p.id !== 'pane-' + S.pane));
};
actions.toggleTheme = function () {
  const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  localStorage.setItem(LS_THEME, next);
  applyTheme(next);
};
actions.doLogout = function () {
  j('POST', '/api/v1/logout', {}).catch(() => { /* 登出失败也继续本地清理 */ });
  stopWatch();
  S.token = '';
  localStorage.removeItem(LS_TOKEN); localStorage.removeItem(LS_ROLE); localStorage.removeItem(LS_PROJ);
  location.reload();
};

/* ---------- 登录 ---------- */
async function doLogin() {
  const pw = $('login-pw').value;
  const user = $('login-user').value.trim();
  if (!pw) { showErr('login-err', '请输入密码'); return; }
  hideErr('login-err');
  const body = { password: pw };
  if (user) body.username = user; // 项目管理员登录（全局管理员留空，请求体与旧版一致）
  await withBusy($('login-submit'), async () => {
    try {
      const r = await j('POST', '/api/v1/login', body);
      S.token = r.token;
      localStorage.setItem(LS_TOKEN, r.token);
      localStorage.setItem(LS_ROLE, r.role || '');
      localStorage.setItem(LS_PROJ, r.project || '');
      $('login-pw').value = '';
      enterApp();
      toast('登录成功');
    } catch (e) {
      showErr('login-err', e.message);
    }
  });
}

function renderSession() {
  let name = '已登录', sub = '', ch = '·';
  if (S.role === 'admin') { name = '管理员'; sub = '全局'; ch = 'A'; }
  else if (S.role === 'project_admin') { name = '项目管理员'; sub = S.roleProject ? '项目 ' + S.roleProject : '项目'; ch = 'P'; }
  $('who-name').textContent = name;
  $('who-sub').textContent = sub;
  $('who-avatar').textContent = ch;
}

function enterApp() {
  S.role = localStorage.getItem(LS_ROLE) || '';
  S.roleProject = localStorage.getItem(LS_PROJ) || '';
  renderSession();
  $('login-view').classList.add('hidden');
  $('app').classList.remove('hidden');
  loadProjects();
}

/* ---------- 项目 ---------- */
async function loadProjects() {
  try {
    S.projects = (await j('GET', '/api/v1/projects')) || [];
    if (!S.projects.some((p) => p.id === S.project)) S.project = S.projects.length ? S.projects[0].id : '';
    renderProjects();
    if (S.project) loadProject();
  } catch (e) {
    if (!e.expired) toast(e.message, 'err');
  }
}

function renderProjects() {
  $('proj-chips').innerHTML = S.projects
    .map((p) => `<button type="button" class="chip${p.id === S.project ? ' active' : ''}" data-act="selectProject" data-id="${esc(p.id)}" title="${esc(p.id)}">${esc(p.name)}</button>`)
    .join('');
  $('btn-del-proj').classList.toggle('hidden', !S.project);
  $('cfg-empty').classList.toggle('hidden', !!S.projects.length);
  $('cfg-work').classList.toggle('hidden', !S.projects.length);
}

actions.selectProject = function (el) {
  const id = el.dataset.id;
  if (!id || id === S.project) return;
  S.project = id;
  S.branch = '';
  S.gray = null;
  stopWatch();
  renderProjects();
  loadProject();
};

actions.newProjectModal = function () {
  openModal({
    title: '新建项目',
    input: true, label: '项目名', placeholder: '小写字母 / 数字 / 连字符，如 mall-order',
    okText: '创建',
    onOk: async (v) => {
      const name = (v || '').trim();
      if (!name) { toast('请输入项目名', 'err'); return; }
      try {
        const r = await j('POST', '/api/v1/projects', { name });
        toast('项目已创建');
        S.project = (r && r.id) || name;
        await loadProjects();
      } catch (e) { toast(e.message, 'err'); }
    },
  });
};

actions.deleteProject = function () {
  if (!S.project) return;
  const pid = S.project;
  openModal({
    title: '删除项目',
    message: `确认删除项目 ${pid}？该项目的全部分支、草稿与版本历史将被移除，操作不可恢复。`,
    okText: '删除', danger: true,
    onOk: async () => {
      try {
        await j('DELETE', '/api/v1/projects/' + pid + '?force=true');
        S.project = '';
        toast('项目已删除');
        loadProjects();
      } catch (e) { toast(e.message, 'err'); }
    },
  });
};

/* ---------- 分支 ---------- */
function fillBranchOptions(sel, bs, keep) {
  const prev = keep ? sel.value : '';
  if (!bs || !bs.length) {
    sel.innerHTML = '<option value="">（无分支）</option>';
    sel.disabled = true;
    return;
  }
  sel.disabled = false;
  sel.innerHTML = bs.map((b) => `<option value="${esc(b.name)}">${esc(b.name)} · v${b.active_version}</option>`).join('');
  if (prev && bs.some((b) => b.name === prev)) sel.value = prev;
}

function renderBranchSelects() {
  fillBranchOptions($('sel-branch'), S.branches, true);
  fillBranchOptions($('diff-a'), S.branches, true);
  fillBranchOptions($('diff-b'), S.branches, true);
  fillBranchOptions($('promote-from'), S.branches, true);
  fillBranchOptions($('promote-to'), S.branches, true);
}

async function loadProject() {
  if (!S.project) return;
  try {
    const [bs, struct] = await Promise.all([
      j('GET', `/api/v1/projects/${S.project}/branches`),
      j('GET', `/api/v1/projects/${S.project}/structure-draft`),
    ]);
    S.branches = bs || [];
    renderBranchSelects();
    // 结构 textarea 自动填充（切换项目或未编辑时；有未保存编辑则不覆盖）
    if (!S.structDirty || S.structProj !== S.project) {
      $('struct-draft').value = JSON.stringify(struct ?? { base_version: null, groups: [] }, null, 2);
      hideErr('struct-err');
      S.structDirty = false;
    }
    S.structProj = S.project;
    if (S.branches.length) {
      const target = S.branches.some((b) => b.name === S.branch) ? S.branch : S.branches[0].name;
      $('sel-branch').value = target;
      loadBranch();
    } else {
      S.branch = '';
      renderNoBranch();
    }
  } catch (e) {
    if (!e.expired) toast(e.message, 'err');
  }
}

function renderNoBranch() {
  S.version = 0; S.structV = 0; S.draftRev = 0; S.gray = null;
  renderCtxBadges();
  $('draft-rev').textContent = 'r0';
  $('draft-groups').innerHTML =
    '<div class="empty mini"><svg class="ic"><use href="#i-branch"/></svg><h4>暂无分支</h4><p>新建分支后即可编辑草稿。</p></div>';
  $('versions-body').innerHTML = '';
  $('gray-summary').innerHTML = '<span class="muted small">选择分支后加载灰度状态</span>';
}

actions.selectBranch = function () { loadBranch(); }; // 仅响应 change（CHANGE_ONLY 过滤了 click）

actions.newBranchModal = function () {
  if (!S.project) return toast('请先选择项目', 'err');
  openModal({
    title: '新建分支',
    input: true, label: '分支名', placeholder: '如 feature-ttl',
    okText: '创建',
    onOk: async (v) => {
      const name = (v || '').trim();
      if (!name) { toast('请输入分支名', 'err'); return; }
      try {
        await j('POST', `/api/v1/projects/${S.project}/branches`, { name });
        toast('分支已创建');
        S.branch = name;
        loadProject();
      } catch (e) { toast(e.message, 'err'); }
    },
  });
};

actions.deleteBranch = function () {
  if (!S.project || !S.branch) return;
  const b = S.branch;
  openModal({
    title: '删除分支',
    message: `确认删除分支 ${S.project}/${b}？该分支的草稿与版本历史将被移除。`,
    okText: '删除', danger: true,
    onOk: async () => {
      try {
        await j('DELETE', `/api/v1/projects/${S.project}/branches/${b}`);
        toast('分支已删除');
        loadProject();
      } catch (e) { toast(e.message, 'err'); }
    },
  });
};

/* ---------- 上下文栏 / 徽章 ---------- */
function renderCtxBadges() {
  let html =
    `<span class="badge ok">稳定版 <span class="mono">v${S.version}</span></span>` +
    `<span class="badge">结构 <span class="mono">sv${S.structV}</span></span>`;
  if (S.gray && S.gray.gray_active) {
    html += `<span class="badge warn"><span class="dot"></span>灰度 <span class="mono">#${S.gray.gray_seq}</span></span>`;
  }
  $('ctx-badges').innerHTML = html;
}

/* ---------- 分支详情 / 草稿编辑 ---------- */
async function loadBranch() {
  const nb = $('sel-branch').value;
  if (!S.project || !nb) return;
  const branchChanged = nb !== S.grayBranch;
  S.branch = nb;
  if (branchChanged) S.grayDirty = false; // 切换分支时才重置灰度规则表单
  S.grayBranch = nb;
  try {
    const b = await j('GET', `/api/v1/projects/${S.project}/branches/${S.branch}`);
    loadVersions();
    S.version = b.active_version || 0;
    S.structV = b.structure_version || 0;
    renderCtxBadges();
    renderDraftEditor(b);
    loadGrayStatus();
    if (S.watchES) { stopWatch(); startWatch(); } // 切换分支后订阅跟随当前分支
  } catch (e) {
    if (!e.expired) toast(e.message, 'err');
  }
}

function draftRowHtml(g, k, v) {
  const type = v && v.type ? v.type : 'string';
  const common = `data-g="${esc(g)}" data-k="${esc(k)}"`;
  let ctl;
  if (type === 'bool') {
    ctl = `<label class="check"><input type="checkbox" class="draft-in" ${common} data-ty="bool" ${v.bool_value === true ? 'checked' : ''}></label>`;
  } else if (type === 'int' || type === 'float') {
    ctl = `<input type="number" step="${type === 'float' ? 'any' : '1'}" class="in mono draft-in" ${common} data-ty="${esc(type)}" value="${esc(type === 'int' ? (v.int_value ?? '') : (v.float_value ?? ''))}">`;
  } else if (type === 'json') {
    ctl = `<textarea class="in mono draft-in" rows="3" ${common} data-ty="json" spellcheck="false">${esc(v.json_value ?? '')}</textarea>`;
  } else if (type === 'array') {
    ctl = `<input class="in mono draft-in" ${common} data-ty="array" value="${esc((v.list_value || []).join(', '))}">`;
  } else if (type === 'secret') {
    // secret 密文不回显：留空 = 不修改；输入 = 提交明文由服务端加密
    ctl = `<input type="password" class="in draft-in" ${common} data-ty="secret" placeholder="已加密 · 留空不修改，输入以更新" autocomplete="new-password">`;
  } else {
    ctl = `<input class="in draft-in" ${common} data-ty="string" value="${esc(v.str_value ?? '')}">`;
  }
  const icon = type === 'secret' ? '<svg class="ic ic-xs"><use href="#i-lock"/></svg>' : '';
  return `<div class="grow">
    <div class="gkey"><span class="mono">${esc(k)}</span></div>
    <div class="gtype"><span class="ty">${icon}${esc(type)}</span></div>
    <div class="gctl">${ctl}</div>
    <button type="button" class="icon-btn danger" data-act="delDraftItem" ${common} title="删除 ${esc(k)}" aria-label="删除 ${esc(k)}"><svg class="ic"><use href="#i-trash"/></svg></button>
  </div>`;
}

function renderDraftEditor(b) {
  // 乐观锁：记录草稿修订号，保存时回传 expected_draft_rev
  S.draftRev = b.draft_rev || 0;
  $('draft-rev').textContent = 'r' + S.draftRev;
  const groups = Object.keys(b.draft || {});
  $('group-list').innerHTML = groups.map((g) => `<option value="${esc(g)}"></option>`).join('');
  if (!groups.length) {
    $('draft-groups').innerHTML =
      '<div class="empty mini"><svg class="ic"><use href="#i-inbox"/></svg><h4>暂无草稿项</h4><p>在结构中定义后发布，或直接在下方添加配置项。</p></div>';
    return;
  }
  $('draft-groups').innerHTML = groups.map((g) => {
    const items = Object.entries(b.draft[g] || {});
    return `<div class="card gcard">
      <div class="gcard-head"><code class="gname">${esc(g)}</code><span class="muted small">${items.length} 项</span></div>
      <div class="grows">${items.map(([k, dv]) => draftRowHtml(g, k, dv.value)).join('')}</div>
    </div>`;
  }).join('');
}

function buildValue(ty, raw) {
  // 非法数值显式报错，不静默置 0
  switch (ty) {
    case 'int': { const n = parseInt(raw, 10); if (Number.isNaN(n)) throw new Error('int 值非法: ' + raw); return { type: 'int', int_value: n }; }
    case 'float': { const n = parseFloat(raw); if (Number.isNaN(n)) throw new Error('float 值非法: ' + raw); return { type: 'float', float_value: n }; }
    case 'bool': return { type: 'bool', bool_value: raw === 'true' || raw === 'on' };
    case 'json': return { type: 'json', json_value: raw };
    case 'array': return { type: 'array', list_value: raw.split(',').map((x) => x.trim()).filter(Boolean) };
    case 'secret': return { type: 'string', str_value: raw };
    default: return { type: 'string', str_value: raw };
  }
}

actions.addDraftItem = async function (el) {
  const g = $('new-item-group').value.trim();
  const k = $('new-item-key').value.trim();
  const ty = $('new-item-type').value;
  const raw = $('new-item-val').value;
  if (!g || !k) { showErr('add-item-err', '组与 key 必填'); return; }
  let value;
  try { value = buildValue(ty, raw); } catch (e) { showErr('add-item-err', e.message); return; }
  hideErr('add-item-err');
  await withBusy(el, async () => {
    try {
      await j('PUT', `/api/v1/projects/${S.project}/branches/${S.branch}/draft`, { updates: [{ group: g, key: k, value }], deletes: [] });
      toast('草稿项已添加');
      loadBranch();
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

actions.delDraftItem = function (el) {
  const g = el.dataset.g, k = el.dataset.k;
  j('PUT', `/api/v1/projects/${S.project}/branches/${S.branch}/draft`, { updates: [], deletes: [g + '/' + k] })
    .then(() => { toast('草稿项已删除'); loadBranch(); })
    .catch((e) => { if (!e.expired) toast(e.message, 'err'); });
};

actions.saveDraft = async function (el) {
  const updates = [];
  let bad = null;
  // 收集全部草稿控件（input / textarea / checkbox / password —— 修复旧版遗漏 json textarea 的问题）
  for (const inp of $$('#pane-draft .draft-in')) {
    if (bad) break;
    const g = inp.dataset.g, k = inp.dataset.k, ty = inp.dataset.ty || 'string';
    if (inp.type === 'password') {
      if (!inp.value) continue; // 留空 = 不修改，服务端保留原密文
      updates.push({ group: g, key: k, value: { type: 'string', str_value: inp.value } });
    } else if (inp.type === 'checkbox') {
      updates.push({ group: g, key: k, value: { type: 'bool', bool_value: inp.checked } });
    } else {
      try { updates.push({ group: g, key: k, value: buildValue(ty, inp.value) }); }
      catch (e) { bad = `${g}/${k}：${e.message}`; }
    }
  }
  if (bad) return toast(bad, 'err');
  await withBusy(el, async () => {
    try {
      // 乐观锁：携带 expected_draft_rev；409 = 草稿已被他人修改
      await j('PUT', `/api/v1/projects/${S.project}/branches/${S.branch}/draft`, { updates, deletes: [], expected_draft_rev: S.draftRev });
      toast('草稿已保存');
      loadBranch();
    } catch (e) {
      if (e.status === 409) {
        toast('草稿已被他人修改，已加载最新版本，请确认后继续', 'warn');
        loadBranch();
      } else if (!e.expired) toast(e.message, 'err');
    }
  });
};

actions.doPublish = function () {
  if (!S.project || !S.branch) return toast('请先选择项目与分支', 'err');
  openModal({
    title: '发布版本',
    message: `将 ${S.project}/${S.branch} 当前草稿发布为新版本。`,
    input: true, label: '备注', placeholder: '备注（可选）',
    okText: '发布',
    onOk: async (comment) => {
      try {
        const r = await j('POST', `/api/v1/projects/${S.project}/branches/${S.branch}/publish`, { comment, request_id: rid() });
        toast('已发布 v' + r.version);
        if (Array.isArray(r.warnings) && r.warnings.length) {
          toast('发布校验警告：' + r.warnings.join('；'), 'warn', 8000);
        }
        loadProject();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

/* ---------- 变更订阅（watch） ---------- */
actions.toggleWatch = function () { if (S.watchES) stopWatch(); else startWatch(); };

function startWatch() {
  if (!S.project || !S.branch) { toast('请先选择项目与分支', 'err'); return; }
  $('watch-panel').classList.remove('hidden');
  $('btn-watch').classList.add('on');
  $('btn-watch-label').textContent = '停止订阅';
  $('watch-ctx').textContent = `${S.project}/${S.branch} · after_version=${S.version}`;
  $('events').textContent = '';
  // 断线重连由浏览器自动进行；after_version 保证续传不丢事件
  S.watchES = new EventSource(`/v1/projects/${S.project}/branches/${S.branch}/watch?after_version=${S.version}`);
  S.watchES.onmessage = (ev) => appendEvent(ev.data);
  S.watchES.onerror = () => { $('events').textContent += '（断线重连…）\n'; };
}
function appendEvent(data) {
  const el = $('events');
  let txt = el.textContent + data + '\n';
  const lines = txt.split('\n'); // 截断保留最近 200 行（内存防护）
  if (lines.length > 200) txt = lines.slice(lines.length - 200).join('\n');
  el.textContent = txt;
  el.scrollTop = el.scrollHeight;
  try { const v = JSON.parse(data).version || 0; if (v > S.version) S.version = v; } catch (_) { /* 非事件负载 */ }
}
function stopWatch() {
  if (S.watchES) { S.watchES.close(); S.watchES = null; }
  $('watch-panel').classList.add('hidden');
  $('btn-watch').classList.remove('on');
  $('btn-watch-label').textContent = '订阅变更';
}

/* ---------- 灰度发布 ---------- */
async function loadGrayStatus() {
  if (!S.project || !S.branch) return;
  try {
    S.gray = await j('GET', `/api/v1/projects/${S.project}/branches/${S.branch}/gray-status`);
  } catch (e) {
    S.gray = { gray_active: false, error: e.message };
  }
  renderGraySummary();
  renderCtxBadges();
  if (!S.grayDirty) populateGrayForm(S.gray && S.gray.gray_rule); // 有未发布编辑时不覆盖表单
}

function grayRuleChips(rule) {
  const chips = [];
  (rule.match_labels || []).forEach((l) => chips.push(`${l.key}=${l.value}`));
  if ((rule.ip_cidrs || []).length) chips.push(`CIDR ×${rule.ip_cidrs.length}`);
  if (rule.percentage !== null && rule.percentage !== undefined) chips.push(`比例 ${rule.percentage}%`);
  return chips.map((c) => `<span class="chip mini">${esc(c)}</span>`).join('');
}

function renderGraySummary() {
  const g = S.gray;
  if (!g) { $('gray-summary').innerHTML = '<span class="muted small">选择分支后加载灰度状态</span>'; return; }
  if (g.error) {
    $('gray-summary').innerHTML = `<span class="badge err">灰度状态不可用</span><span class="muted small">${esc(g.error)}</span>`;
    return;
  }
  const meta = `稳定版 <span class="mono">v${g.active_version}</span> · 结构 <span class="mono">sv${g.structure_version}</span>`;
  if (g.gray_active) {
    $('gray-summary').innerHTML =
      `<div class="gs-left">
        <span class="badge warn"><span class="dot"></span>灰度进行中</span>
        <span class="badge acc">序号 <span class="mono">#${g.gray_seq}</span></span>
        <span class="muted small">${meta}</span>
        <div class="gs-chips">${grayRuleChips(g.gray_rule || {})}</div>
      </div>
      <div class="gs-actions">
        <button type="button" class="btn primary" data-act="doGrayPromote"><svg class="ic"><use href="#i-up"/></svg>一键转正</button>
        <button type="button" class="btn danger" data-act="doGrayAbort"><svg class="ic"><use href="#i-rollback"/></svg>一键下量</button>
      </div>`;
  } else {
    $('gray-summary').innerHTML =
      `<div class="gs-left"><span class="badge">灰度未启用</span><span class="muted small">${meta}</span></div>
       <div class="gs-actions"><span class="muted small">编辑规则并发布灰度后，可在此转正或下量</span></div>`;
  }
}

function labelRowHtml(l) {
  return `<div class="label-row">
    <input class="in mono" data-lf="key" placeholder="key（如 zone）" value="${esc(l.key || '')}">
    <span class="muted small">=</span>
    <input class="in mono" data-lf="value" placeholder="value（如 cn-north-1）" value="${esc(l.value || '')}">
    <button type="button" class="icon-btn danger" data-act="removeLabelRow" title="移除此标签" aria-label="移除此标签"><svg class="ic"><use href="#i-x"/></svg></button>
  </div>`;
}

function populateGrayForm(rule) {
  rule = rule || {};
  const labels = (rule.match_labels || []).filter(Boolean);
  $('label-rows').innerHTML = (labels.length ? labels : [{}]).map(labelRowHtml).join('');
  $('gray-cidrs').value = (rule.ip_cidrs || []).join('\n');
  const pct = rule.percentage;
  $('gray-pct').value = (pct === null || pct === undefined) ? '' : String(pct);
  if (!$('gray-rule').classList.contains('hidden')) {
    $('gray-rule').value = JSON.stringify(rule, null, 2); // JSON 模式下同步
  }
}

function grayRuleFromForm() {
  const labels = $$('#label-rows .label-row').map((r) => ({
    key: r.querySelector('[data-lf="key"]').value.trim(),
    value: r.querySelector('[data-lf="value"]').value.trim(),
  })).filter((l) => l.key || l.value);
  const cidrs = $('gray-cidrs').value.split(/[\n,]/).map((x) => x.trim()).filter(Boolean);
  const raw = $('gray-pct').value.trim();
  const pct = raw === '' ? null : Number(raw);
  return { match_labels: labels, ip_cidrs: cidrs, percentage: pct };
}

function grayJsonMode() { return !$('gray-rule').classList.contains('hidden'); }

actions.toggleGrayJson = function () {
  if (grayJsonMode()) {
    let rule;
    try { rule = JSON.parse($('gray-rule').value || '{}'); }
    catch (e) { showErr('gray-err', 'JSON 非法，无法转回表单：' + e.message); return; }
    hideErr('gray-err');
    populateGrayForm(rule);
    $('gray-rule').classList.add('hidden');
    $('gray-form').classList.remove('hidden');
    $('btn-gray-mode-label').textContent = 'JSON 模式';
  } else {
    $('gray-rule').value = JSON.stringify(grayRuleFromForm(), null, 2);
    hideErr('gray-err');
    $('gray-form').classList.add('hidden');
    $('gray-rule').classList.remove('hidden');
    $('gray-rule').focus();
    $('btn-gray-mode-label').textContent = '表单模式';
  }
};

actions.addLabelRow = function () {
  $('label-rows').insertAdjacentHTML('beforeend', labelRowHtml({}));
  const rows = $$('#label-rows .label-row');
  rows[rows.length - 1].querySelector('[data-lf="key"]').focus();
};
actions.removeLabelRow = function (el) {
  const row = el.closest('.label-row');
  if (row) row.remove();
};

actions.loadGrayRule = async function (el) {
  if (!S.project || !S.branch) return toast('请先选择项目与分支', 'err');
  await withBusy(el, async () => {
    try {
      const g = await j('GET', `/api/v1/projects/${S.project}/branches/${S.branch}/gray-status`);
      S.gray = g;
      renderGraySummary(); renderCtxBadges();
      populateGrayForm(g.gray_rule || { match_labels: [], ip_cidrs: [], percentage: null });
      S.grayDirty = false; // 显式载入 = 以服务端规则为准
      toast('已载入当前规则');
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

actions.doGrayPublish = function () {
  if (!S.project || !S.branch) return toast('请先选择项目与分支', 'err');
  let rule;
  if (grayJsonMode()) {
    try { rule = JSON.parse($('gray-rule').value); }
    catch (e) { showErr('gray-err', '规则 JSON 非法：' + e.message); return; }
  } else {
    rule = grayRuleFromForm();
    if (rule.match_labels.some((l) => !l.key || !l.value)) { showErr('gray-err', '标签的 key 与 value 需同时填写'); return; }
    if (rule.ip_cidrs.some((c) => !c.includes('/'))) { showErr('gray-err', 'CIDR 需为「地址/前缀」形式，如 10.0.0.0/8'); return; }
    if (rule.percentage !== null && (Number.isNaN(rule.percentage) || rule.percentage < 0 || rule.percentage > 100)) { showErr('gray-err', '百分比范围为 0–100'); return; }
    if (!rule.match_labels.length && !rule.ip_cidrs.length && rule.percentage === null) {
      showErr('gray-err', '规则至少需要一个判据：标签 / IP CIDR / 百分比'); return;
    }
  }
  hideErr('gray-err');
  openModal({
    title: '发布灰度',
    message: `将 ${S.project}/${S.branch} 当前草稿固化为灰度快照；稳定版不变，命中规则的客户端读灰度快照。`,
    input: true, label: '备注', placeholder: '备注（可选）',
    okText: '发布灰度',
    onOk: async (comment) => {
      try {
        const r = await j('POST', `/api/v1/projects/${S.project}/branches/${S.branch}/gray-publish`, { rule, comment, request_id: rid() });
        toast('灰度已发布 #seq=' + r.gray_seq);
        S.grayDirty = false;
        loadGrayStatus();
        loadProject();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

actions.doGrayPromote = function () {
  if (!S.project || !S.branch) return toast('请先选择项目与分支', 'err');
  openModal({
    title: '灰度转正',
    message: `将 ${S.project}/${S.branch} 的灰度内容发布为新稳定版，全量客户端切换到灰度内容。`,
    input: true, label: '备注', placeholder: '备注（可选）',
    okText: '转正',
    onOk: async (comment) => {
      try {
        const r = await j('POST', `/api/v1/projects/${S.project}/branches/${S.branch}/gray-promote`, { comment, request_id: rid() });
        toast('已转正，新稳定版 v' + r.active_version);
        S.grayDirty = false;
        loadGrayStatus();
        loadProject();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

actions.doGrayAbort = function () {
  if (!S.project || !S.branch) return toast('请先选择项目与分支', 'err');
  openModal({
    title: '灰度下量',
    message: `摘除 ${S.project}/${S.branch} 的灰度指针，灰度客户端回落到稳定版。`,
    input: true, label: '备注', placeholder: '备注（可选）',
    okText: '下量', danger: true,
    onOk: async (comment) => {
      try {
        const r = await j('POST', `/api/v1/projects/${S.project}/branches/${S.branch}/gray-abort`, { comment, request_id: rid() });
        toast('已下量，客户端回落稳定版 v' + r.fallback_version);
        S.grayDirty = false;
        loadGrayStatus();
        loadProject();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

/* ---------- 版本历史 / 回滚 ---------- */
async function loadVersions() {
  if (!S.project || !S.branch) return;
  try {
    const vs = await j('GET', `/api/v1/projects/${S.project}/branches/${S.branch}/versions`);
    renderVersions(vs);
  } catch (e) { if (!e.expired) toast(e.message, 'err'); }
}
actions.refreshVersions = function () { loadVersions(); };

function versionBadges(v) {
  let html = '';
  if (v.rollback_of) html += `<span class="badge warn">回滚 ← v${v.rollback_of}</span> `;
  if (v.gray) html += '<span class="badge acc">灰度</span> ';
  html += v.kind === 'diff' ? '<span class="badge">增量</span>' : '<span class="badge">完整</span>';
  return html;
}

function renderVersions(vs) {
  const tb = $('versions-body');
  if (!vs || !vs.length) {
    tb.innerHTML = '<tr><td colspan="7"><div class="empty mini"><svg class="ic"><use href="#i-history"/></svg><h4>暂无版本记录</h4><p>发布版本后在此生成历史，可随时回滚。</p></div></td></tr>';
    return;
  }
  tb.innerHTML = vs.slice().sort((a, b) => b.no - a.no).map((v) => `<tr>
    <td class="mono tnum">v${v.no}</td>
    <td>${versionBadges(v)}</td>
    <td class="cmt">${esc(v.comment || '—')}</td>
    <td class="mono muted tnum">sv${v.structure_version}</td>
    <td>${esc(v.operator || '—')}</td>
    <td class="muted small nowrap">${fmtTime(v.created_at)}</td>
    <td class="nowrap"><button type="button" class="btn sm ghost" data-act="doRollback" data-ver="${v.no}" title="回滚到此版本"><svg class="ic ic-xs"><use href="#i-rollback"/></svg>回滚</button></td>
  </tr>`).join('');
}

actions.doRollback = function (el) {
  const toVersion = Number(el.dataset.ver);
  openModal({
    title: '回滚到 v' + toVersion,
    message: `以 v${toVersion} 的内容创建一个新版本（历史不可变），当前草稿保持不变。`,
    input: true, label: '备注', placeholder: '备注（可选）',
    okText: '回滚', danger: true,
    onOk: async (comment) => {
      try {
        const r = await j('POST', `/api/v1/projects/${S.project}/branches/${S.branch}/rollback`, { to_version: toVersion, comment, request_id: rid() });
        toast('已回滚，新版本 v' + r.new_version);
        loadProject();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

/* ---------- 结构 ---------- */
actions.loadStructDraft = async function (el) {
  if (!S.project) return;
  await withBusy(el, async () => {
    try {
      const d = await j('GET', `/api/v1/projects/${S.project}/structure-draft`);
      $('struct-draft').value = JSON.stringify(d, null, 2);
      hideErr('struct-err');
      S.structDirty = false; // 显式载入 = 以服务端为准
      toast('已载入当前结构');
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

function parseStruct() {
  try { return { ok: true, data: JSON.parse($('struct-draft').value) }; }
  catch (e) { return { ok: false, err: e.message }; }
}

actions.saveStructDraft = async function (el) {
  const p = parseStruct();
  if (!p.ok) { showErr('struct-err', '结构 JSON 非法：' + p.err); return; }
  hideErr('struct-err');
  await withBusy(el, async () => {
    try {
      await j('PUT', `/api/v1/projects/${S.project}/structure-draft`, p.data);
      S.structDirty = false; // 已保存，textarea 与服务端一致
      toast('结构草稿已保存');
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

actions.publishStruct = function () {
  if (!S.project) return;
  const p = parseStruct();
  if (!p.ok) { showErr('struct-err', '结构 JSON 非法：' + p.err); return; }
  hideErr('struct-err');
  openModal({
    title: '发布结构',
    message: '发布结构将推进全部分支的版本，订阅客户端会收到结构变更事件。',
    input: true, label: '备注', placeholder: '备注（可选）',
    okText: '发布结构',
    onOk: async (comment) => {
      try {
        const r = await j('POST', `/api/v1/projects/${S.project}/structure-draft/publish`, { comment, request_id: rid() });
        const s = JSON.stringify(r);
        toast('结构已发布 ' + (s.length > 90 ? s.slice(0, 90) + '…' : s));
        loadProject();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

/* ---------- 配置预览 ---------- */
actions.openCfgModal = function () {
  if (!S.project || !S.branch) return toast('请先选择项目与分支', 'err');
  $('cfgm-ctx').textContent = `${S.project} / ${S.branch}`;
  $('cfg-overlay').classList.remove('hidden');
  fetchCfg();
};
actions.closeCfg = function () { $('cfg-overlay').classList.add('hidden'); };
actions.cfgReveal = function () { fetchCfg(); };          // 仅响应 change
actions.cfgFormat = function () { fetchCfg(); };          // 仅响应 change
actions.refreshCfg = function () { fetchCfg(); };

async function fetchCfg() {
  const out = $('cfg-out');
  const reveal = $('cfg-reveal').checked;
  $('cfg-format').disabled = reveal;
  out.textContent = '加载中…';
  try {
    if (reveal) {
      // 明文走管理面（会话鉴权 + 审计），返回 JSON
      const d = await j('GET', `/api/v1/projects/${S.project}/branches/${S.branch}/config?reveal=true`);
      out.textContent = JSON.stringify(d, null, 2);
    } else {
      // 默认走数据面（secret 掩码 ***），支持 YAML / JSON / TOML 渲染
      const fmt = $('cfg-format').value;
      out.textContent = await jtext(`/v1/projects/${S.project}/branches/${S.branch}/config?format=${fmt}`);
    }
  } catch (e) {
    out.textContent = '';
    if (!e.expired) toast(e.message, 'err');
  }
}

actions.copyCfg = async function () {
  const t = $('cfg-out').textContent || '';
  if (!t || t === '加载中…') return;
  try {
    await navigator.clipboard.writeText(t);
    toast('已复制到剪贴板');
  } catch (_) {
    try {
      const ta = document.createElement('textarea');
      ta.value = t;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      ta.remove();
      toast('已复制到剪贴板');
    } catch (_2) { toast('复制失败，请手动选择复制', 'err'); }
  }
};

/* ---------- 分支对比 / 提升 ---------- */
actions.showDiff = async function (el) {
  const a = $('diff-a').value, b = $('diff-b').value;
  if (!a || !b) return toast('请选择对比分支', 'err');
  await withBusy(el, async () => {
    try {
      const d = await j('GET', `/api/v1/projects/${S.project}/diff?branch_a=${encodeURIComponent(a)}&branch_b=${encodeURIComponent(b)}`);
      renderDiff(d, a, b);
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

function renderDiff(d, a, b) {
  const diffs = d.diffs || [], missing = d.missing || [];
  if (!diffs.length && !missing.length) {
    $('diff-out').innerHTML = '<div class="empty mini"><svg class="ic"><use href="#i-check"/></svg><h4>两个分支完全一致</h4><p>没有发现值差异或缺失项。</p></div>';
    return;
  }
  let html = `<div class="table-wrap"><table class="table">
    <thead><tr><th>key</th><th>${esc(a)}</th><th>${esc(b)}</th></tr></thead><tbody>`;
  for (const x of diffs) {
    html += `<tr><td class="mono">${esc(x.group)}/${esc(x.key)}</td><td class="mono brk">${esc(JSON.stringify(x.branch_a))}</td><td class="mono brk">${esc(JSON.stringify(x.branch_b))}</td></tr>`;
  }
  for (const m of missing) {
    html += `<tr><td class="mono">${esc(m)}</td><td colspan="2" class="muted">仅一侧有值</td></tr>`;
  }
  html += '</tbody></table></div>';
  $('diff-out').innerHTML = html;
}

actions.doPromote = async function (el) {
  const from = $('promote-from').value, to = $('promote-to').value;
  if (!from || !to) return toast('请选择提升源 / 目标分支', 'err');
  if (from === to) return toast('源与目标分支不能相同', 'err');
  await withBusy(el, async () => {
    try {
      const r = await j('POST', `/api/v1/projects/${S.project}/promote`, { from, to, force: $('promote-force').checked });
      toast(`提升完成：写入 ${r.applied.length} 项，跳过 ${r.skipped.length} 项，源缺失 ${r.missing_from.length} 项`);
      if (r.skipped.length) toast('已跳过（目标草稿已修改，可勾选 force 覆盖）：' + r.skipped.join('、'), 'warn', 8000);
      loadBranch();
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

/* ---------- 共享库 ---------- */
async function loadShared() {
  $('shared-body').innerHTML = '<tr><td colspan="6">' + skeleton(4) + '</td></tr>';
  try {
    const [pub, draft] = await Promise.all([
      j('GET', '/api/v1/shared').catch(() => []),
      j('GET', '/api/v1/shared-draft').catch(() => []),
    ]);
    const rows = (draft || []).map((x) => ({ ...x, __draft: true }))
      .concat((pub || []).map((x) => ({ ...x, __draft: false })));
    if (!rows.length) {
      $('shared-body').innerHTML = '<tr><td colspan="6"><div class="empty mini"><svg class="ic"><use href="#i-shared"/></svg><h4>暂无共享项</h4><p>在上方表单创建共享草稿，发布后自动级联引用它的项目分支。</p></div></td></tr>';
      return;
    }
    $('shared-body').innerHTML = rows.map((x) => `<tr>
      <td class="mono">${esc(x.group)}</td>
      <td class="mono">${esc(x.key)}</td>
      <td class="mono muted">${esc(x.ty || x.type || '')}</td>
      <td>${x.__draft ? '<span class="badge warn">草稿</span>' : `<span class="badge ok">v${x.version}</span>`}</td>
      <td class="mono brk">${esc(JSON.stringify(x.value))}</td>
      <td>${x.secret ? '<span class="badge err"><svg class="ic ic-xs"><use href="#i-lock"/></svg>secret</span>' : ''}</td>
    </tr>`).join('');
  } catch (e) {
    if (!e.expired) { $('shared-body').innerHTML = ''; toast(e.message, 'err'); }
  }
}
actions.refreshShared = function () { loadShared(); };

actions.saveShared = async function (el) {
  const group = $('sh-group').value.trim();
  const key = $('sh-key').value.trim();
  if (!group || !key) { showErr('sh-err', '组与 key 必填'); return; }
  let value;
  try { value = JSON.parse($('sh-value').value); }
  catch (e) { showErr('sh-err', '值 JSON 非法：' + e.message); return; }
  hideErr('sh-err');
  const body = { group, key, type: $('sh-type').value, secret: $('sh-secret').checked, required: $('sh-required').checked, value };
  await withBusy(el, async () => {
    try {
      await j('POST', '/api/v1/shared', body);
      toast('共享草稿已保存');
      loadShared();
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

actions.publishShared = function () {
  openModal({
    title: '发布共享',
    message: '发布全部共享草稿；引用这些共享项的项目分支将自动级联生成新版本。',
    input: true, label: '备注', placeholder: '备注（可选）',
    okText: '发布共享',
    onOk: async (comment) => {
      try {
        const r = await j('POST', '/api/v1/shared/publish', { comment, request_id: rid() });
        toast(`共享已发布 v${r.version}，级联 ${ (r.affected || []).length } 个分支`);
        loadShared();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

actions.bindRef = async function (el) {
  const body = {
    project: $('ref-project').value.trim(),
    group: $('ref-group').value.trim(),
    item_key: $('ref-item').value.trim() || null,
    shared_group: $('ref-sg').value.trim(),
    shared_key: $('ref-sk').value.trim(),
  };
  if (!body.project || !body.group || !body.shared_group || !body.shared_key) { showErr('ref-err', '项目 / 结构组 / 共享组 / 共享 key 必填'); return; }
  hideErr('ref-err');
  await withBusy(el, async () => {
    try {
      await j('POST', '/api/v1/shared/refs', body);
      toast('引用已绑定');
    } catch (e) { if (!e.expired) toast(e.message, 'err'); }
  });
};

/* ---------- 审计 ---------- */
async function loadAudit() {
  $('audit-body').innerHTML = '<tr><td colspan="8">' + skeleton(6) + '</td></tr>';
  try {
    const f = $('audit-filter').value.trim();
    const es = await j('GET', '/api/v1/audit?limit=200' + (f ? '&action=' + encodeURIComponent(f) : ''));
    if (!es.length) {
      $('audit-body').innerHTML = '<tr><td colspan="8"><div class="empty mini"><svg class="ic"><use href="#i-audit"/></svg><h4>暂无审计记录</h4><p>调整过滤条件或刷新查看最新操作。</p></div></td></tr>';
      return;
    }
    $('audit-body').innerHTML = es.map((x) => `<tr>
      <td class="mono muted tnum">${x.seq}</td>
      <td class="muted small nowrap">${fmtTime(x.ts)}</td>
      <td class="mono">${esc(x.action)}</td>
      <td class="mono">${esc(x.project || '')}${x.branch ? '/' + esc(x.branch) : ''}</td>
      <td class="mono tnum">${esc(x.version ?? '')}</td>
      <td>${esc(x.operator || '')}</td>
      <td class="mono muted small">${esc(x.request_id || '')}</td>
      <td class="mono brk small">${esc(JSON.stringify(x.detail || {}))}</td>
    </tr>`).join('');
  } catch (e) {
    if (!e.expired) { $('audit-body').innerHTML = ''; toast(e.message, 'err'); }
  }
}
actions.refreshAudit = function () { loadAudit(); };

/* ---------- 集群 ---------- */
async function loadCluster() {
  const box = $('cluster-out');
  box.innerHTML = '<div class="card">' + skeleton(3) + '</div>';
  try {
    const m = await j('GET', '/api/v1/cluster/members');
    const members = m.members || [];
    let html = `<div class="card cluster-meta">本节点 <code>${esc(m.node_id ?? 'dev-single')}</code> · 状态 <code>${esc(m.state)}</code> · 当前 Leader <code>${esc(m.current_leader ?? '—')}</code></div>`;
    if (members.length) {
      html += `<div class="card"><div class="table-wrap"><table class="table">
        <thead><tr><th>node_id</th><th>HTTP</th><th>gRPC</th><th>角色</th><th></th></tr></thead><tbody>
        ${members.map((x) => `<tr>
          <td class="mono tnum">${esc(x.node_id)}</td>
          <td class="mono">${esc(x.http_addr || '—')}</td>
          <td class="mono">${esc(x.grpc_addr || '—')}</td>
          <td>${x.is_voter ? '<span class="badge acc">voter</span>' : '<span class="badge">learner</span>'}${x.is_leader ? ' <span class="badge ok">leader</span>' : ''}</td>
          <td class="nowrap">${x.is_voter
            ? `<button type="button" class="btn sm ghost danger" data-act="removeNode" data-node="${esc(x.node_id)}" data-http="${esc(x.http_addr || '')}" data-raft="${esc(x.raft_addr || '')}">移除</button>`
            : `<button type="button" class="btn sm" data-act="promoteNode" data-node="${esc(x.node_id)}" data-http="${esc(x.http_addr || '')}" data-raft="${esc(x.raft_addr || '')}"><svg class="ic ic-xs"><use href="#i-up"/></svg>提升为 voter</button>`}</td>
        </tr>`).join('')}
      </tbody></table></div></div>`;
    } else {
      html += '<div class="card empty"><svg class="ic"><use href="#i-cluster"/></svg><h4>无集群成员</h4><p>当前为单节点模式，没有 Raft 成员。集群模式下可在此查看与管理节点。</p></div>';
    }
    box.innerHTML = html;
  } catch (e) {
    if (e.expired) return;
    box.innerHTML = e.message.includes('404')
      ? '<div class="card empty"><svg class="ic"><use href="#i-cluster"/></svg><h4>单节点模式</h4><p>dev-single 模式没有集群管理；以集群模式启动后可在此查看成员、提升与移除节点。</p></div>'
      : `<div class="card empty"><svg class="ic"><use href="#i-alert"/></svg><h4>无法加载集群信息</h4><p>${esc(e.message)}（项目管理员无集群权限）</p></div>`;
  }
}
actions.refreshCluster = function () { loadCluster(); };

actions.promoteNode = function (el) {
  const { node, http, raft } = el.dataset;
  openModal({
    title: '提升节点',
    message: `将节点 ${node} 提升为 voter，参与 Raft 共识投票。`,
    okText: '提升',
    onOk: async () => {
      try {
        await j('POST', '/api/v1/cluster/promote', { node_id: Number(node), http_addr: http || '', raft_addr: raft || '' });
        toast('节点已提升');
        loadCluster();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

actions.removeNode = function (el) {
  const { node } = el.dataset;
  openModal({
    title: '移除节点',
    message: `将节点 ${node} 从成员表移除；移除前请确认该节点已安全停机。`,
    okText: '移除', danger: true,
    onOk: async () => {
      try {
        await j('POST', '/api/v1/cluster/remove', { node_id: Number(node) });
        toast('节点已移除');
        loadCluster();
      } catch (e) { if (!e.expired) toast(e.message, 'err'); }
    },
  });
};

/* ============================================================
   事件绑定与启动
   ============================================================ */
const CHANGE_ONLY = new Set(['selectBranch', 'cfgFormat', 'cfgReveal']); // 仅响应 change，避免 click 误触发

function bindEvents() {
  // data-act 委托（D-CSP：无 onclick 属性）
  document.addEventListener('click', (e) => {
    const el = e.target.closest('[data-act]');
    if (!el || el.disabled) return;
    const name = el.dataset.act;
    if (CHANGE_ONLY.has(name)) return;
    const fn = actions[name];
    if (typeof fn === 'function') fn.call(el, el, e);
  });
  document.addEventListener('change', (e) => {
    const el = e.target.closest('[data-act]');
    if (!el || el.disabled) return;
    const fn = actions[el.dataset.act];
    if (typeof fn === 'function') fn.call(el, el, e);
  });

  // 登录（Enter 提交）
  $('login-form').addEventListener('submit', (e) => { e.preventDefault(); doLogin(); });

  // 弹窗
  $('modal-ok').addEventListener('click', () => {
    const v = $('modal-input').value;
    const cb = modalCb;
    closeModal();
    if (cb) cb(v);
  });
  $('modal-cancel').addEventListener('click', closeModal);
  $('modal-overlay').addEventListener('mousedown', (e) => { if (e.target === $('modal-overlay')) closeModal(); });
  $('modal-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); $('modal-ok').click(); }
  });
  $('cfg-overlay').addEventListener('mousedown', (e) => { if (e.target === $('cfg-overlay')) actions.closeCfg(); });

  // Esc 关闭弹窗
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (!$('modal-overlay').classList.contains('hidden')) closeModal();
    else if (!$('cfg-overlay').classList.contains('hidden')) actions.closeCfg();
  });

  // 审计过滤（Enter）
  $('audit-filter').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); loadAudit(); }
  });

  // 未保存编辑标记（结构 / 灰度规则）：有输入时后台刷新不覆盖
  $('struct-draft').addEventListener('input', () => { S.structDirty = true; });
  $('pane-gray').addEventListener('input', (e) => {
    if (e.target.closest('#gray-form') || e.target.id === 'gray-rule') S.grayDirty = true;
  });

  // 会话心跳（5 分钟）
  setInterval(() => {
    if (S.token) j('POST', '/api/v1/heartbeat', {}).catch(() => { /* 心跳失败不打扰 */ });
  }, 300000);
}

function boot() {
  initTheme();
  bindEvents();
  S.token = localStorage.getItem(LS_TOKEN) || '';
  S.role = localStorage.getItem(LS_ROLE) || '';
  S.roleProject = localStorage.getItem(LS_PROJ) || '';
  if (S.token) enterApp();
  else $('login-view').classList.remove('hidden');
}

boot();
