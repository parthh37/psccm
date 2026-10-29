'use strict';

// ---------------------------------------------------------------- helpers

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

const store = {
  get(k, d) { try { const v = localStorage.getItem('psccm_' + k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem('psccm_' + k, JSON.stringify(v)); } catch { /* private mode */ } },
};

let TOKEN = store.get('token', '');

async function api(method, path, body) {
  const headers = { 'Content-Type': 'application/json' };
  if (TOKEN) headers.Authorization = 'Bearer ' + TOKEN;
  const r = await fetch('/api' + path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  if (r.status === 401) { askToken(); throw new Error('Access token required'); }
  const ct = r.headers.get('content-type') || '';
  const data = ct.includes('json') ? await r.json() : await r.text();
  if (!r.ok) throw new Error((data && data.error) || data || r.statusText);
  return data;
}
const GET = (p) => api('GET', p);
const POST = (p, b = {}) => api('POST', p, b);
const PUT = (p, b) => api('PUT', p, b);
const DEL = (p) => api('DELETE', p);

function toast(msg, bad = false) {
  const t = document.createElement('div');
  t.className = 'toast' + (bad ? ' bad' : '');
  t.textContent = msg;
  $('#toast').appendChild(t);
  setTimeout(() => t.remove(), bad ? 7000 : 3500);
}

/** Wrap an async UI action: shows errors as a toast. */
function act(fn) {
  return async (...a) => {
    try { return await fn(...a); } catch (e) { toast(e.message, true); return false; }
  };
}

function modal(title, body, buttons = [], opts = {}) {
  const root = $('#modal');
  root.innerHTML = `<div class="modal-bg"><div class="modal ${opts.wide ? 'wide' : ''}">
    <div class="modal-head"><h3>${esc(title)}</h3><button class="x" data-close>&times;</button></div>
    <div class="modal-body">${body}</div>
    <div class="modal-foot"></div></div></div>`;
  const close = () => { root.innerHTML = ''; };
  const foot = $('.modal-foot', root);
  buttons.forEach((b) => {
    const el = document.createElement('button');
    el.textContent = b.label;
    if (b.cls) el.className = b.cls;
    el.onclick = async () => {
      if (!b.onClick) return close();
      el.disabled = true;
      try { if ((await b.onClick(root)) !== false) close(); } catch (e) { toast(e.message, true); }
      el.disabled = false;
    };
    foot.appendChild(el);
  });
  if (!buttons.length) foot.remove();
  $('[data-close]', root).onclick = close;
  $('.modal-bg', root).addEventListener('mousedown', (e) => { if (e.target.classList.contains('modal-bg')) close(); });
  const first = $('input, textarea, select', root);
  if (first) first.focus();
  return { root, close };
}

function askToken() {
  if ($('#token-input')) return;
  modal('Access token', `<p>This console is protected. Enter the token it was started with (<code>--token</code>).</p>
    <input id="token-input" type="password" autocomplete="current-password">`, [
    { label: 'Save', cls: 'primary', onClick: () => { TOKEN = $('#token-input').value.trim(); store.set('token', TOKEN); route(); } },
  ]);
}

function fmtTime(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  if (isNaN(d)) return iso;
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function ago(iso) {
  if (!iso) return '';
  const s = (Date.now() - new Date(iso).getTime()) / 1000;
  if (s < 0) return 'in ' + dur(-s);
  if (s < 60) return 'just now';
  return dur(s) + ' ago';
}

function dur(s) {
  if (s < 60) return Math.round(s) + 's';
  if (s < 3600) return Math.round(s / 60) + 'm';
  if (s < 86400) return Math.round(s / 3600) + 'h';
  return Math.round(s / 86400) + 'd';
}

function elapsed(a, b) {
  if (!a) return '';
  const s = ((b ? new Date(b) : new Date()) - new Date(a)) / 1000;
  return s < 60 ? Math.round(s) + 's' : Math.floor(s / 60) + 'm ' + Math.round(s % 60) + 's';
}

const pill = (s) => `<span class="pill s-${esc(s)}">${esc(s)}</span>`;
const osLabel = (o) => (o === 'windows' ? 'Windows' : o === 'linux' ? 'Linux' : '?');
const tagsHtml = (t) => (t || []).map((x) => `<span class="tag">${esc(x)}</span>`).join('');
const sizeFmt = (b) => (b > 1048576 ? (b / 1048576).toFixed(1) + ' MB' : Math.max(1, Math.round(b / 1024)) + ' KB');

function progress(d) {
  const t = Math.max(d.n_total, 1);
  return `<div class="progress" title="${d.n_success} ok, ${d.n_skipped} skipped, ${d.n_failed} failed of ${d.n_total}">
    <span class="p-ok" style="width:${(100 * d.n_success) / t}%"></span>
    <span class="p-skip" style="width:${(100 * d.n_skipped) / t}%"></span>
    <span class="p-bad" style="width:${(100 * d.n_failed) / t}%"></span></div>`;
}

const ICON = {
  dashboard: '<path d="M3 13h8V3H3zm0 8h8v-6H3zm10 0h8V11h-8zm0-18v6h8V3z"/>',
  deploy: '<path d="M12 2l4 4h-3v7h-2V6H8zm-8 14h2v4h12v-4h2v6H4z"/>',
  history: '<path d="M13 3a9 9 0 1 0 8.94 10h-2.02A7 7 0 1 1 13 5V2l4 4-4 4V7a5 5 0 1 0 5 5h2a7 7 0 0 1-7 7zM12 8h2v5l4 2-1 1.7-5-2.7z"/>',
  computers: '<path d="M3 4h18v12H3zm2 2v8h14V6zM8 18h8v2H8z"/>',
  packages: '<path d="M12 2l9 5v10l-9 5-9-5V7zm0 2.3L5.5 8 12 11.6 18.5 8zM5 9.7v6.1l6 3.4v-6.2zm8 9.5l6-3.4V9.7l-6 3.3z"/>',
  catalog: '<path d="M7 7V6a5 5 0 0 1 10 0v1h4l-1 15H4L3 7zm2 0h6V6a3 3 0 0 0-6 0zM5.1 9l.8 11h12.2l.8-11z"/>',
  inventory: '<path d="M9 2h6a1 1 0 0 1 1 1v1h3v18H5V4h3V3a1 1 0 0 1 1-1zm1 2v2h4V4zM7 6v14h10V6h-1v2H8V6zm2 5h6v2H9zm0 4h6v2H9z"/>',
  gpo: '<path d="M12 2l8 3v6c0 5-3.4 9.4-8 11-4.6-1.6-8-6-8-11V5zm0 2.2L6 6.4V11c0 3.9 2.5 7.4 6 8.9 3.5-1.5 6-5 6-8.9V6.4zM11 7h2v6h-2zm0 8h2v2h-2z"/>',
  schedules: '<path d="M7 2h2v2h6V2h2v2h3v18H4V4h3zm-1 8v10h12V10zm0-4v2h12V6z"/>',
  settings: '<path d="M12 8a4 4 0 1 1 0 8 4 4 0 0 1 0-8zm8.6 5.5l1.9 1.5-2 3.4-2.3-.9a7.7 7.7 0 0 1-2.1 1.2L15.8 21h-4l-.4-2.4a7.7 7.7 0 0 1-2.1-1.2l-2.3.9-2-3.4L7 13.4a7.8 7.8 0 0 1 0-2.8L5 9.1l2-3.4 2.3.9a7.7 7.7 0 0 1 2.1-1.2L11.8 3h4l.4 2.4a7.7 7.7 0 0 1 2.1 1.2l2.3-.9 2 3.4-1.9 1.5a7.8 7.8 0 0 1 0 2.9z"/>',
  agents: '<path d="M4 4h16v10H4zm2 2v6h12V6zm-3 11h18v2H3zm9-9a2 2 0 1 1 0 4 2 2 0 0 1 0-4z"/>',
  updates: '<path d="M12 4a8 8 0 0 1 7.4 5H22l-3.5 4L15 9h2.2A6 6 0 0 0 6.3 10.1L4.4 9.4A8 8 0 0 1 12 4zm-7.5 7L8 15H5.8a6 6 0 0 0 10.9 1.9l1.9.7A8 8 0 0 1 3.6 15H1zm6.5-3h2v4.6l3 1.8-1 1.7-4-2.4z"/>',
};

const NAV = [
  ['dashboard', 'Dashboard'], ['deploy', 'Deploy'], ['history', 'Deployments'], ['computers', 'Computers'],
  ['agents', 'Agents'], ['inventory', 'Inventory'], ['updates', 'Updates'], ['packages', 'Packages'], ['catalog', 'App catalog'], ['gpo', 'Group Policy'], ['schedules', 'Schedules'], ['settings', 'Settings'],
];

// ------------------------------------------------------------------ router

let pollTimer = null;
function stopPolling() { if (pollTimer) clearTimeout(pollTimer); pollTimer = null; }
function poll(fn, ms) { stopPolling(); pollTimer = setTimeout(fn, ms); }

async function route() {
  stopPolling();
  $('#modal').innerHTML = '';
  const parts = location.hash.replace(/^#\/?/, '').split('/');
  const name = parts[0] || 'dashboard';
  const param = decodeURIComponent(parts.slice(1).join('/') || '');
  const navKey = { deployment: 'history', package: 'packages', agent: 'agents' }[name] || name;
  $('#nav').innerHTML = NAV.map(([k, label]) =>
    `<a href="#/${k}" class="${k === navKey ? 'active' : ''}"><svg viewBox="0 0 24 24" fill="currentColor">${ICON[k]}</svg>${label}</a>`).join('');
  const main = $('#main');
  const fn = PAGES[name] || PAGES.dashboard;
  try {
    await fn(main, param);
  } catch (e) {
    main.innerHTML = `<div class="card"><div class="card-body err">${esc(e.message)}</div></div>`;
  }
}
window.addEventListener('hashchange', route);

// --------------------------------------------------------------- dashboard

async function pageDashboard(el) {
  const s = await GET('/summary');
  const steps = [
    [s.credentials > 0, 'Add a credential', 'Windows: leave the user empty to use the account running PSCCM (Kerberos). Linux: an SSH user with sudo.', '#/settings'],
    [s.computers > 0, 'Add computers', 'Type names, or import them read-only from Active Directory.', '#/computers'],
    [s.recent.length > 0, 'Deploy something', 'Pick a package (7-Zip, Chrome, Node.js, IIS, HAProxy…), pick computers, press Deploy.', '#/deploy'],
  ];
  const allDone = steps.every((x) => x[0]);
  el.innerHTML = `
    <div class="page-head"><h1>Dashboard</h1>
      <div class="actions">
        <button onclick="location.hash='#/computers'">Computers</button>
        <button class="primary" onclick="location.hash='#/deploy'">New deployment</button>
      </div></div>
    <div class="stats">
      <div class="stat"><div class="n">${s.computers}</div><div class="l">Computers (${s.windows} Windows, ${s.linux} Linux)</div></div>
      <div class="stat"><div class="n" style="color:var(--ok)">${s.online}</div><div class="l">Online at last check</div></div>
      <div class="stat"><div class="n" style="color:${s.offline ? 'var(--bad)' : 'inherit'}">${s.offline}</div><div class="l">Offline at last check</div></div>
      <div class="stat"><div class="n">${s.packages}</div><div class="l">Packages</div></div>
      <div class="stat"><div class="n" style="color:${s.running ? 'var(--info)' : 'inherit'}">${s.running}</div><div class="l">Running deployments</div></div>
    </div>
    ${s.package_errors.length ? `<div class="card"><div class="card-head"><h2 class="err">Packages with errors</h2></div><div class="card-body">
      ${s.package_errors.map(([f, e]) => `<div><b>${esc(f)}</b>: <span class="err">${esc(e)}</span></div>`).join('')}</div></div>` : ''}
    <div class="grid two">
      ${allDone ? '' : `<div class="card"><div class="card-head"><h2>Getting started</h2></div><div class="card-body"><ul class="checklist">
        ${steps.map(([done, t, d, href]) => `<li><span class="ck ${done ? 'done' : ''}">${done ? '✓' : ''}</span>
          <div><a href="${href}"><b>${t}</b></a><div class="muted small">${d}</div></div></li>`).join('')}
      </ul></div></div>`}
      <div class="card" style="${allDone ? 'grid-column:1/-1' : ''}"><div class="card-head"><h2>Recent deployments</h2>
        <div class="right"><a href="#/history">All deployments</a></div></div>
        <div class="card-body flush">${deploymentTable(s.recent)}</div></div>
    </div>`;
  if (s.running) poll(() => pageDashboard(el).catch(() => {}), 4000);
}

function deploymentTable(list) {
  if (!list.length) return '<div class="empty"><b>No deployments yet</b>Start one from the Deploy page.</div>';
  return `<div class="table-wrap"><table><thead><tr><th>Package</th><th>Status</th><th>Progress</th><th>Targets</th><th>Started</th><th>By</th></tr></thead><tbody>
    ${list.map((d) => `<tr class="click" onclick="location.hash='#/deployment/${d.id}'">
      <td><b>${esc(d.package_name)}</b></td>
      <td>${pill(d.status)}${d.scheduled_for && d.status === 'scheduled' ? ` <span class="small muted">${fmtTime(d.scheduled_for)}</span>` : ''}</td>
      <td>${progress(d)}</td>
      <td class="num small">${d.n_success} ok · ${d.n_skipped} skip · <span class="${d.n_failed ? 'err' : ''}">${d.n_failed} fail</span> / ${d.n_total}</td>
      <td class="small" title="${fmtTime(d.created_at)}">${ago(d.started_at || d.created_at)}</td>
      <td class="small muted">${esc(d.source)}</td></tr>`).join('')}
    </tbody></table></div>`;
}

// --------------------------------------------------------------- computers

const CS = { list: [], creds: [], groups: [], adGroups: [], q: '', os: '', tag: '', group: '', sel: new Set() };

async function pageComputers(el) {
  let g;
  [CS.list, CS.creds, g] = await Promise.all([GET('/computers'), GET('/credentials'), GET('/groups')]);
  CS.groups = g.groups;
  CS.adGroups = g.ad_groups;
  if (CS.group && !CS.groups.some((x) => x.name === CS.group)) CS.group = '';
  CS.sel = new Set([...CS.sel].filter((n) => CS.list.some((c) => c.name === n)));
  const tags = [...new Set(CS.list.flatMap((c) => c.tags))].sort();
  const cur = CS.groups.find((x) => x.name === CS.group);
  el.innerHTML = `
    <div class="page-head"><h1>Computers</h1><span class="sub">${CS.list.length} total</span>
      <div class="actions">
        <button onclick="App.checkOnline()">Check online</button>
        <button onclick="App.adImport()">Import from AD</button>
        <button class="primary" onclick="App.addComputers()">Add computers</button>
      </div></div>
    <div class="card"><div class="card-head"><h2>Groups</h2>
      <div class="chips" style="flex:1">
        <span class="chip ${!CS.group ? 'on' : ''}" onclick="App.cGroup('')">All computers <span class="small">${CS.list.length}</span></span>
        ${CS.groups.map((x) => `<span class="chip ${CS.group === x.name ? 'on' : ''}" onclick="App.cGroup(${jstr(x.name)})" title="${esc(x.description)}">
          ${x.kind === 'dynamic' ? '⚡ ' : ''}${esc(x.name)} <span class="small">${x.members.length}</span></span>`).join('')}
      </div>
      <div class="right">
        ${cur ? `<button onclick="App.editGroup(${cur.id})">Edit group</button><button class="danger" onclick="App.delGroup(${cur.id})">Delete group</button>` : ''}
        <button onclick="App.editGroup()">New group</button></div></div>
      ${cur ? `<div class="card-body small muted">${cur.kind === 'dynamic' ? `Dynamic group: ${esc(ruleText(cur.rules))}` : 'Static group: add computers by selecting them below and choosing "Add to group".'}
        ${cur.description ? ' · ' + esc(cur.description) : ''}</div>` : ''}
    </div>
    <div class="card">
      <div class="card-head">
        <input class="search" placeholder="Search name, OU, description…" value="${esc(CS.q)}" oninput="App.cFilter('q', this.value)">
        <select style="width:auto" onchange="App.cFilter('os', this.value)">
          <option value="">All OS</option><option value="windows" ${CS.os === 'windows' ? 'selected' : ''}>Windows</option>
          <option value="linux" ${CS.os === 'linux' ? 'selected' : ''}>Linux</option><option value="unknown" ${CS.os === 'unknown' ? 'selected' : ''}>Unknown</option></select>
        <select style="width:auto" onchange="App.cFilter('tag', this.value)"><option value="">All tags</option>
          ${tags.map((t) => `<option ${CS.tag === t ? 'selected' : ''}>${esc(t)}</option>`).join('')}</select>
        <div class="right" id="c-selbar"></div>
      </div>
      <div class="card-body flush" id="c-table"></div>
    </div>`;
  renderComputerTable();
}

function ruleText(r) {
  const parts = [];
  if (r.os) parts.push('OS ' + osLabel(r.os));
  if (r.name) parts.push('name ' + r.name);
  if (r.ou) parts.push('OU contains ' + r.ou);
  if (r.os_detail) parts.push('OS contains ' + r.os_detail);
  if (r.tags && r.tags.length) parts.push('tag ' + r.tags.join('/'));
  if (r.ad_group) parts.push('AD group ' + r.ad_group);
  if (r.app) parts.push((r.app_missing ? 'without ' : 'has ') + r.app + (r.app_version ? ' ' + r.app_version : ''));
  if (r.pending_reboot) parts.push('reboot pending');
  if (r.low_disk_gb) parts.push(`a disk under ${r.low_disk_gb} GB free`);
  if (r.missing_updates) parts.push(r.missing_updates === 'security' ? 'missing security updates' : 'missing updates');
  return parts.join(' and ');
}

function filteredComputers() {
  const q = CS.q.toLowerCase();
  const g = CS.groups.find((x) => x.name === CS.group);
  const inGroup = g ? new Set(g.members.map((m) => m.toLowerCase())) : null;
  return CS.list.filter((c) => (!CS.os || c.os === CS.os) && (!CS.tag || c.tags.includes(CS.tag)) &&
    (!inGroup || inGroup.has(c.name.toLowerCase())) &&
    (!q || [c.name, c.ad_ou, c.description, c.os_detail, c.address].some((v) => (v || '').toLowerCase().includes(q))));
}

function renderComputerTable() {
  const list = filteredComputers();
  const credName = (id) => (CS.creds.find((c) => c.id === id) || {}).name;
  const allSel = list.length && list.every((c) => CS.sel.has(c.name));
  $('#c-table').innerHTML = !CS.list.length
    ? '<div class="empty"><b>No computers yet</b>Add them by name or import from Active Directory.</div>'
    : `<div class="table-wrap"><table><thead><tr>
      <th style="width:30px"><input type="checkbox" ${allSel ? 'checked' : ''} onchange="App.cSelAll(this.checked)"></th>
      <th>Name</th><th>OS</th><th>Status</th><th>Tags / groups</th><th>Source / OU</th><th>Credential</th><th></th></tr></thead><tbody>
      ${list.map((c) => {
        const groups = CS.groups.filter((g) => g.members.some((m) => m.toLowerCase() === c.name.toLowerCase())).map((g) => g.name);
        return `<tr class="${CS.sel.has(c.name) ? 'sel' : ''}">
        <td><input type="checkbox" ${CS.sel.has(c.name) ? 'checked' : ''} onchange="App.cSel(${jstr(c.name)}, this.checked)"></td>
        <td><b>${esc(c.name)}</b>${c.address ? `<div class="small muted">${esc(c.address)}</div>` : ''}${c.description ? `<div class="small muted">${esc(c.description)}</div>` : ''}</td>
        <td>${osLabel(c.os)}<div class="os">${esc(c.os_detail || '')}</div></td>
        <td class="small"><span class="dot ${c.online === true ? 'on' : c.online === false ? 'off' : ''}"></span>${c.online === true ? 'online' : c.online === false ? 'offline' : 'not checked'}
          ${c.last_check ? `<div class="muted">${ago(c.last_check)}</div>` : ''}</td>
        <td>${tagsHtml(c.tags)}${groups.map((g) => `<span class="tag on">${esc(g)}</span>`).join('')}</td>
        <td class="small">${esc(c.source)}${c.ad_ou ? `<div class="muted" title="${esc(c.ad_ou)}">${esc(c.ad_ou.split(',').slice(0, 2).join(','))}</div>` : ''}</td>
        <td class="small">${esc(credName(c.credential_id) || 'default')}</td>
        <td><button onclick="App.editComputer(${c.id})">Edit</button></td></tr>`;
      }).join('')}
      </tbody></table></div>
      ${list.length === 0 ? '<div class="empty">No computers match the filter.</div>' : ''}`;
  const n = CS.sel.size;
  const statics = CS.groups.filter((g) => g.kind === 'static');
  const cur = CS.groups.find((x) => x.name === CS.group);
  $('#c-selbar').innerHTML = n ? `<span class="small muted">${n} selected</span>
    ${statics.length ? `<select style="width:auto" onchange="App.addToGroup(this.value); this.value=''"><option value="">Add to group…</option>
      ${statics.map((g) => `<option value="${g.id}">${esc(g.name)}</option>`).join('')}</select>` : ''}
    ${cur && cur.kind === 'static' ? `<button onclick="App.removeFromGroup(${cur.id})">Remove from group</button>` : ''}
    <button onclick="App.checkOnline(true)">Check</button>
    <button class="danger" onclick="App.deleteComputers()">Remove</button>
    <button class="primary" onclick="App.deployTo()">Deploy to ${n}</button>` : '';
}

function credOptions(sel, kinds, emptyLabel = 'Default for its OS') {
  return `<option value="">${esc(emptyLabel)}</option>` + CS.creds.filter((c) => !kinds || kinds.includes(c.kind))
    .map((c) => `<option value="${c.id}" ${c.id === sel ? 'selected' : ''}>${esc(c.name)} (${esc(c.kind)}${c.username ? ', ' + esc(c.username) : ''})</option>`).join('');
}

const App = {
  cFilter(k, v) { CS[k] = v; renderComputerTable(); },
  cSel(name, on) { on ? CS.sel.add(name) : CS.sel.delete(name); renderComputerTable(); },
  cSelAll(on) { filteredComputers().forEach((c) => (on ? CS.sel.add(c.name) : CS.sel.delete(c.name))); renderComputerTable(); },

  addComputers() {
    modal('Add computers', `
      <div class="field"><label>Computer names or IP addresses</label>
        <textarea id="ac-names" rows="6" placeholder="web01.corp.local&#10;web02.corp.local&#10;10.0.0.15"></textarea>
        <div class="hint">One per line, or separated by commas. Use the FQDN for WinRM/Kerberos.</div></div>
      <div class="row">
        <div class="field"><label>Operating system</label><select id="ac-os">
          <option value="">Detect automatically</option><option value="windows">Windows</option><option value="linux">Linux</option></select></div>
        <div class="field"><label>Tags</label><input id="ac-tags" placeholder="web, prod"></div>
      </div>
      <div class="field"><label>Credential</label><select id="ac-cred">${credOptions(null, ['windows', 'ssh'])}</select></div>`,
    [{ label: 'Cancel' }, {
      label: 'Add', cls: 'primary', onClick: async () => {
        const r = await POST('/computers', {
          names: $('#ac-names').value, os: $('#ac-os').value || null,
          tags: $('#ac-tags').value.split(',').map((s) => s.trim()).filter(Boolean),
          credential_id: +$('#ac-cred').value || null,
        });
        toast(`Added ${r.added} computer(s)`);
        route();
      },
    }]);
  },

  editComputer(id) {
    const c = CS.list.find((x) => x.id === id);
    modal('Edit ' + c.name, `
      <div class="row">
        <div class="field"><label>Operating system</label><select id="ec-os">
          ${['unknown', 'windows', 'linux'].map((o) => `<option value="${o}" ${c.os === o ? 'selected' : ''}>${o === 'unknown' ? 'Detect automatically' : osLabel(o)}</option>`).join('')}</select></div>
        <div class="field"><label>Port</label><input id="ec-port" type="number" placeholder="${c.os === 'linux' ? 22 : 5985}" value="${c.port || ''}"></div>
      </div>
      <div class="field"><label>Connect to (address)</label><input id="ec-addr" value="${esc(c.address || '')}" placeholder="${esc(c.name)}">
        <div class="hint">Leave empty to connect by name.</div></div>
      <div class="field"><label>Tags</label><input id="ec-tags" value="${esc(c.tags.join(', '))}"></div>
      <div class="field"><label>Credential</label><select id="ec-cred">${credOptions(c.credential_id, ['windows', 'ssh'])}</select></div>
      <div class="field"><label>Description</label><input id="ec-desc" value="${esc(c.description || '')}"></div>`,
    [{ label: 'Cancel' }, {
      label: 'Save', cls: 'primary', onClick: async () => {
        await PUT('/computers/' + id, {
          os: $('#ec-os').value, port: +$('#ec-port').value || null, address: $('#ec-addr').value.trim() || null,
          tags: $('#ec-tags').value.split(',').map((s) => s.trim()).filter(Boolean),
          credential_id: +$('#ec-cred').value || null, description: $('#ec-desc').value.trim() || null,
        });
        route();
      },
    }]);
  },

  deleteComputers: act(async () => {
    const ids = CS.list.filter((c) => CS.sel.has(c.name)).map((c) => c.id);
    if (!confirm(`Remove ${ids.length} computer(s) from the list? (Nothing is changed on the computers or in AD.)`)) return;
    await POST('/computers/delete', { ids });
    CS.sel.clear();
    route();
  }),

  checkOnline: act(async (onlySelected) => {
    toast('Checking…');
    const r = await POST('/computers/check', { names: onlySelected ? [...CS.sel] : null });
    toast(`${r.online} of ${r.checked} online`);
    route();
  }),

  deployTo() {
    W.computers = new Set(CS.sel);
    location.hash = '#/deploy';
  },

  async adImport() {
    const m = modal('Import from Active Directory', '<div class="muted">Querying Active Directory (read-only)…</div>', [{ label: 'Close' }], { wide: true });
    let list;
    try {
      list = (await POST('/ad/preview', {})).computers;
    } catch (e) {
      $('.modal-body', m.root).innerHTML = `<p class="err">${esc(e.message)}</p><p>Check the Active Directory settings in <a href="#/settings" onclick="document.querySelector('#modal').innerHTML=''">Settings</a>.</p>`;
      return;
    }
    const sel = new Set(list.filter((c) => !c.exists).map((c) => c.name));
    const draw = () => {
      $('.modal-body', m.root).innerHTML = `
        <div class="row" style="margin-bottom:10px"><div class="field" style="margin:0"><input id="ad-q" placeholder="Filter…" class="search"></div>
          <div class="small muted">${list.length} found · ${list.filter((c) => !c.exists).length} new · <b>${sel.size}</b> selected</div>
          <button onclick="App.adSelAll(true)">Select all</button><button onclick="App.adSelAll(false)">None</button></div>
        <div class="table-wrap" style="max-height:55vh;overflow:auto"><table><thead><tr><th></th><th>Name</th><th>OS</th><th>OU</th><th>Last logon</th><th></th></tr></thead><tbody>
        ${list.map((c, i) => `<tr data-name="${esc(c.name.toLowerCase())} ${esc((c.ou || '').toLowerCase())} ${esc(c.os.toLowerCase())}">
          <td><input type="checkbox" data-i="${i}" ${sel.has(c.name) ? 'checked' : ''}></td>
          <td><b>${esc(c.name)}</b>${c.description ? `<div class="small muted">${esc(c.description)}</div>` : ''}</td>
          <td class="small">${esc(c.os)} ${esc(c.os_version)}</td>
          <td class="small muted">${esc(c.ou.split(',').filter((p) => /^OU=/i.test(p)).map((p) => p.slice(3)).join(' / ') || c.ou)}</td>
          <td class="small">${c.last_logon ? ago(c.last_logon) : '-'}</td>
          <td>${c.exists ? '<span class="tag">in list</span>' : '<span class="pill s-running">new</span>'}${c.enabled ? '' : ' <span class="tag">disabled</span>'}</td></tr>`).join('')}
        </tbody></table></div>`;
      $$('input[data-i]', m.root).forEach((cb) => { cb.onchange = () => { const c = list[+cb.dataset.i]; cb.checked ? sel.add(c.name) : sel.delete(c.name); }; });
      $('#ad-q', m.root).oninput = (e) => {
        const q = e.target.value.toLowerCase();
        $$('tbody tr', m.root).forEach((tr) => { tr.style.display = tr.dataset.name.includes(q) ? '' : 'none'; });
      };
    };
    App.adSelAll = (on) => { list.forEach((c) => (on ? sel.add(c.name) : sel.delete(c.name))); draw(); };
    draw();
    const foot = $('.modal-foot', m.root);
    const b = document.createElement('button');
    b.className = 'primary';
    b.textContent = 'Import selected';
    b.onclick = act(async () => {
      const r = await POST('/ad/import', { computers: list.filter((c) => sel.has(c.name)) });
      toast(`Imported ${r.imported} computer(s)`);
      m.close();
      route();
    });
    foot.appendChild(b);
  },
};
window.App = App;

Object.assign(App, {
  cGroup(name) { CS.group = name; route(); },
  addToGroup: act(async (id) => {
    if (!id) return;
    const r = await POST(`/groups/${id}/members`, { add: [...CS.sel] });
    toast(`Added ${r.added} computer(s) to the group`);
    route();
  }),
  removeFromGroup: act(async (id) => {
    const r = await POST(`/groups/${id}/members`, { remove: [...CS.sel] });
    toast(`Removed ${r.removed} computer(s) from the group`);
    CS.sel.clear();
    route();
  }),
  delGroup: act(async (id) => {
    const g = CS.groups.find((x) => x.id === id);
    if (!confirm(`Delete group '${g.name}'? The computers stay in the list.`)) return;
    await DEL('/groups/' + id);
    CS.group = '';
    route();
  }),
  /** `preset` pre-fills a new group (e.g. from the Inventory page). */
  editGroup(id, preset) {
    const g = CS.groups.find((x) => x.id === id) || { id: 0, name: '', description: '', kind: 'static', rules: { tags: [] }, members: [], ...preset };
    const r = g.rules || {};
    const m = modal(id ? 'Edit group ' + g.name : 'New group', `
      <div class="row"><div class="field"><label>Name</label><input id="g-name" value="${esc(g.name)}" placeholder="Web servers"></div>
        <div class="field"><label>Description</label><input id="g-desc" value="${esc(g.description)}"></div></div>
      <div class="field"><label>Type</label><div class="tabs">
        <button type="button" data-kind="static" class="${g.kind === 'static' ? 'on' : ''}">Static — pick computers</button>
        <button type="button" data-kind="dynamic" class="${g.kind === 'dynamic' ? 'on' : ''}">Dynamic — by rules</button></div></div>
      <div id="g-static">
        ${id ? '<p class="small muted">Add or remove members by selecting computers in the list and using "Add to group" / "Remove from group".</p>'
          : `<div class="field"><label>Computers</label><textarea id="g-members" rows="4" placeholder="One per line">${esc([...CS.sel].join('\n'))}</textarea>
            <div class="hint">Pre-filled with the computers you selected.</div></div>`}
      </div>
      <div id="g-dynamic">
        <p class="small muted" style="margin-top:0">Members are every computer matching <b>all</b> filled-in rules, re-evaluated each time the group is used — new AD computers join automatically.</p>
        <div class="row">
          <div class="field"><label>Operating system</label><select id="gr-os"><option value="">Any</option>
            <option value="windows" ${r.os === 'windows' ? 'selected' : ''}>Windows</option><option value="linux" ${r.os === 'linux' ? 'selected' : ''}>Linux</option></select></div>
          <div class="field"><label>Name pattern</label><input id="gr-name" value="${esc(r.name || '')}" placeholder="web*, app0?"><div class="hint">* and ?, comma separated</div></div>
        </div>
        <div class="row">
          <div class="field"><label>AD OU contains</label><input id="gr-ou" value="${esc(r.ou || '')}" placeholder="OU=Web Servers"></div>
          <div class="field"><label>OS version contains</label><input id="gr-osd" value="${esc(r.os_detail || '')}" placeholder="Server 2022"></div>
        </div>
        <div class="row">
          <div class="field"><label>AD security group</label><input id="gr-ad" list="gr-ad-list" value="${esc(r.ad_group || '')}" placeholder="IIS-Servers">
            <datalist id="gr-ad-list">${CS.adGroups.map((a) => `<option value="${esc(a)}">`).join('')}</datalist>
            <div class="hint">Groups seen on imported AD computers.</div></div>
          <div class="field"><label>Has tag</label><input id="gr-tags" value="${esc((r.tags || []).join(', '))}" placeholder="prod"></div>
        </div>
        <div class="cat">From inventory <span style="text-transform:none;letter-spacing:0;font-weight:400">— only scanned computers match these</span></div>
        <div class="row">
          <div class="field"><label>Installed software</label><input id="gr-app" value="${esc(r.app || '')}" placeholder="Google Chrome*"><div class="hint">Program name, * and ?</div></div>
          <div class="field"><label>Version</label><input id="gr-appv" value="${esc(r.app_version || '')}" placeholder="<130 or >=22.11"><div class="hint">Empty = any version.</div></div>
        </div>
        <label class="check"><input type="checkbox" id="gr-appm" ${r.app_missing ? 'checked' : ''}> Computers that do <b>not</b> have this software (at this version)</label>
        <div class="row" style="align-items:end">
          <div class="field"><label>A disk has less free space than (GB)</label><input id="gr-disk" type="number" min="0" step="any" value="${r.low_disk_gb || ''}"></div>
          <div class="field"><label class="check"><input type="checkbox" id="gr-reboot" ${r.pending_reboot ? 'checked' : ''}> Reboot pending</label></div>
        </div>
        <div class="row">
          <div class="field"><label>Windows updates missing</label><select id="gr-wu">
            ${[['', 'Don\'t care'], ['any', 'Any update'], ['security', 'Security or critical updates']].map(([v, l]) => `<option value="${v}" ${(r.missing_updates || '') === v ? 'selected' : ''}>${l}</option>`).join('')}</select>
            <div class="hint">From the last scan on the Updates page. Declined and optional updates don't count.</div></div>
        </div>
        <div class="small" id="g-preview"></div>
      </div>`,
    [{ label: 'Cancel' }, {
      label: 'Save', cls: 'primary', onClick: async (root) => {
        const kind = $('[data-kind].on', root).dataset.kind;
        const body = {
          id: g.id, name: $('#g-name').value.trim(), description: $('#g-desc').value.trim(), kind, rules: rules(),
          members: kind === 'static' && !id ? $('#g-members').value.split(/[\s,;]+/).filter(Boolean) : [],
        };
        await POST('/groups', body);
        CS.group = body.name;
        if (preset && location.hash !== '#/computers') location.hash = '#/computers';
        else route();
      },
    }]);
    const root = m.root;
    const rules = () => ({
      os: $('#gr-os').value || null, name: $('#gr-name').value.trim(), ou: $('#gr-ou').value.trim(),
      os_detail: $('#gr-osd').value.trim(), ad_group: $('#gr-ad').value.trim(),
      tags: $('#gr-tags').value.split(',').map((s) => s.trim()).filter(Boolean),
      app: $('#gr-app').value.trim(), app_version: $('#gr-appv').value.trim(), app_missing: $('#gr-appm').checked,
      pending_reboot: $('#gr-reboot').checked, low_disk_gb: +$('#gr-disk').value || 0, missing_updates: $('#gr-wu').value,
    });
    let timer;
    const preview = () => {
      clearTimeout(timer);
      timer = setTimeout(async () => {
        if (!$('#g-preview')) return; // dialog closed
        const body = rules();
        try {
          const r2 = await POST('/groups/preview', body);
          const out = $('#g-preview');
          if (!out) return;
          out.innerHTML = r2.members.length
            ? `<b>${r2.members.length}</b> computer(s) match: <span class="muted">${esc(r2.members.slice(0, 12).join(', '))}${r2.members.length > 12 ? '…' : ''}</span>`
            : '<span class="muted">No computers match yet.</span>';
        } catch (e) { if ($('#g-preview')) $('#g-preview').textContent = e.message; }
      }, 250);
    };
    const kindVis = () => {
      const k = $('[data-kind].on', root).dataset.kind;
      $('#g-static').style.display = k === 'static' ? '' : 'none';
      $('#g-dynamic').style.display = k === 'dynamic' ? '' : 'none';
      if (k === 'dynamic') preview();
    };
    $$('[data-kind]', root).forEach((b) => { b.onclick = () => { $$('[data-kind]', root).forEach((x) => x.classList.toggle('on', x === b)); kindVis(); }; });
    $$('#g-dynamic input, #g-dynamic select', root).forEach((i) => { i.oninput = preview; i.onchange = preview; });
    kindVis();
  },
});

// ---------------------------------------------------------------- packages

let PKGS = null;
async function loadPackages(force) {
  if (!PKGS || force) PKGS = await GET('/packages');
  return PKGS;
}

function groupByCategory(list) {
  const groups = {};
  list.forEach((p) => { (groups[p.category] = groups[p.category] || []).push(p); });
  return Object.entries(groups);
}

const platformText = (p) => (p.platforms.length ? p.platforms.map(osLabel).join(' + ') : 'Windows + Linux');

async function pagePackages(el) {
  const [{ packages, errors }, repo] = await Promise.all([loadPackages(true), GET('/repository')]);
  el.innerHTML = `
    <div class="page-head"><h1>Packages</h1><span class="sub">${packages.length} packages</span>
      <div class="actions"><button class="primary" onclick="App.newPackage()">New package</button></div></div>
    ${errors.length ? `<div class="card"><div class="card-head"><h2 class="err">Files that could not be loaded</h2></div><div class="card-body">
      ${errors.map(([f, e]) => `<div><b>${esc(f)}</b>: <span class="err">${esc(e)}</span></div>`).join('')}</div></div>` : ''}
    <div class="card"><div class="card-body">
      ${groupByCategory(packages).map(([cat, list]) => `<div class="cat">${esc(cat)}</div><div class="pkg-grid">
        ${list.map((p) => `<div class="pkg" onclick="location.hash=${jstr('#/package/' + p.id)}">
          <div class="t">${esc(p.name)}</div><div class="d">${esc(p.description)}</div>
          <div class="m">${platformText(p)} · ${p.steps} step${p.steps === 1 ? '' : 's'}${p.builtin ? ' · built-in' : ''}</div></div>`).join('')}
      </div>`).join('')}
    </div></div>
    <div class="card"><div class="card-head"><h2>Repository</h2><span class="small muted">Files packages can push to computers (install: file / copy: src)</span></div>
      <div class="card-body"><p class="small">Folder: <code>${esc(repo.dir)}</code> — copy installers and zip files here (subfolders are fine).</p>
      ${repo.files.length ? `<table><tbody>${repo.files.map((f) => `<tr><td><code>${esc(f.path)}</code></td><td class="small muted num">${sizeFmt(f.size)}</td></tr>`).join('')}</tbody></table>`
        : '<div class="muted small">The repository is empty.</div>'}</div></div>`;
}

const NEW_PACKAGE = `id: my-package
name: My package
category: Custom
description: What this package does.
platforms: [windows]
variables:
  version:
    default: "1.0"
    description: Version to install
# Skip computers where this exits 0 (already installed):
detect:
  windows: |
    if (Test-AppInstalled -Name 'My App*' -MinVersion '{{version}}') { exit 0 }
    exit 1
steps:
  - name: Install My App {{version}}
    install:
      file: apps/myapp-{{version}}.msi   # in the repository folder
      args: ALLUSERS=1
  - name: Restart service
    service:
      name: MyAppService
      action: restart
`;

App.newPackage = (yaml = NEW_PACKAGE) => {
  modal('New package', `<p class="small muted">Write the package in YAML. See the built-in packages for examples; the
    reference is in the README.</p><textarea id="np-yaml" class="code" spellcheck="false">${esc(yaml)}</textarea>`,
  [{ label: 'Cancel' }, {
    label: 'Create', cls: 'primary', onClick: async () => {
      const r = await POST('/packages', { yaml: $('#np-yaml').value });
      PKGS = null;
      location.hash = '#/package/' + r.id;
    },
  }], { wide: true });
};

async function pagePackage(el, id) {
  const { yaml, package: p } = await GET('/packages/' + encodeURIComponent(id));
  const vars = Object.entries(p.variables || {});
  el.innerHTML = `
    <div class="page-head"><h1>${esc(p.name)}</h1><span class="sub">${esc(p.id)}${p.version ? ' · ' + esc(p.version) : ''}</span>
      <div class="actions">
        <button onclick="location.hash='#/packages'">Back</button>
        <button onclick="App.dupPackage()">Duplicate</button>
        <button class="danger" onclick="App.delPackage(${jstr(p.id)})">Delete</button>
        <button class="primary" onclick="App.deployPackage(${jstr(p.id)})">Deploy</button>
      </div></div>
    <div class="split">
      <div>
        <div class="card"><div class="card-body">
          <p style="margin-top:0">${esc(p.description)}</p>
          <div class="small muted">Category: ${esc(p.category || 'Other')} · Runs on: ${platformText(p)}</div>
        </div></div>
        <div class="card"><div class="card-head"><h2>Variables</h2></div><div class="card-body flush">
          ${vars.length ? `<table><thead><tr><th>Name</th><th>Default</th><th>Description</th></tr></thead><tbody>
            ${vars.map(([k, v]) => `<tr><td><code>${esc(k)}</code>${v.required ? ' <span class="err small">required</span>' : ''}</td>
              <td class="small"><code>${esc(v.secret ? '••••' : v.default ?? '')}</code>${v.options && v.options.length ? `<div class="muted">${esc(v.options.join(' | '))}</div>` : ''}</td>
              <td class="small">${esc(v.description)}</td></tr>`).join('')}</tbody></table>` : '<div class="empty">No variables</div>'}
        </div></div>
        <div class="card"><div class="card-head"><h2>Steps</h2></div><div class="card-body flush"><table><tbody>
          ${p.detect && (p.detect.windows || p.detect.linux) ? '<tr><td class="muted">0</td><td><b>Detect</b> <span class="small muted">skip computers that already have it</span></td><td></td></tr>' : ''}
          ${p.steps.map((s, i) => `<tr><td class="muted">${i + 1}</td><td><b>${esc(s.name)}</b>${s.delegate_to ? ` <span class="small muted">on ${esc(s.delegate_to)}</span>` : ''}</td>
            <td class="small muted">${esc(stepKind(s))}${s.os ? ' · ' + osLabel(s.os) : ''}${s.when ? ' · conditional' : ''}</td></tr>`).join('')}
          ${p.verify && (p.verify.windows || p.verify.linux) ? '<tr><td class="muted">✓</td><td><b>Verify</b></td><td></td></tr>' : ''}
          ${(p.rollback || []).length ? `<tr><td class="muted">↺</td><td><b>Rollback on failure</b> <span class="small muted">${p.rollback.length} step(s)</span></td><td></td></tr>` : ''}
        </tbody></table></div></div>
      </div>
      <div class="card"><div class="card-head"><h2>Definition (YAML)</h2><div class="right">
          <span id="pk-status" class="small"></span>
          <button onclick="App.validatePackage()">Validate</button>
          <button class="primary" onclick="App.savePackage(${jstr(p.id)})">Save</button></div></div>
        <div class="card-body"><textarea id="pk-yaml" class="code" spellcheck="false">${esc(yaml)}</textarea></div></div>
    </div>`;
  $('#pk-yaml').addEventListener('keydown', (e) => {
    if (e.key === 'Tab') { e.preventDefault(); document.execCommand('insertText', false, '  '); }
    if ((e.ctrlKey || e.metaKey) && e.key === 's') { e.preventDefault(); App.savePackage(p.id); }
  });
}

function stepKind(s) {
  if (s.script) return s.shell || 'script';
  if (s.install) return 'install ' + (s.install.file || s.install.url || '');
  if (s.copy) return 'copy → ' + s.copy.dest;
  if (s.service) return `service ${s.service.action} ${s.service.name}`;
  if (s.reboot) return 'reboot';
  if (s.wait) return `wait ${s.wait}s`;
  if (s.windows_update) return 'windows update';
  if (s.include) return 'include ' + s.include;
  return '';
}

Object.assign(App, {
  validatePackage: act(async () => {
    const r = await POST('/packages/validate', { yaml: $('#pk-yaml').value });
    $('#pk-status').innerHTML = r.ok ? `<span style="color:var(--ok)">✓ valid (${r.steps} steps)</span>` : `<span class="err">${esc(r.error)}</span>`;
  }),
  savePackage: act(async (id) => {
    await PUT('/packages/' + encodeURIComponent(id), { yaml: $('#pk-yaml').value });
    PKGS = null;
    toast('Saved');
    route();
  }),
  dupPackage() {
    const y = $('#pk-yaml').value.replace(/^id:\s*(\S+)/m, 'id: $1-copy').replace(/^name:\s*(.+)$/m, 'name: $1 (copy)');
    App.newPackage(y);
  },
  delPackage: act(async (id) => {
    if (!confirm(`Delete package '${id}'? The file is renamed to .yaml.disabled.`)) return;
    await DEL('/packages/' + encodeURIComponent(id));
    PKGS = null;
    location.hash = '#/packages';
  }),
  deployPackage(id) {
    W.mode = 'package';
    W.pkg = id;
    W.vars = {};
    location.hash = '#/deploy';
  },
});

// ------------------------------------------------------------------ deploy

const W = {
  mode: 'package', pkg: null, q: '', command: '', shell: '', vars: {},
  computers: new Set(), tags: new Set(), groups: new Set(), extra: '', cq: '', cos: '',
  opt: { concurrency: 10, batch_size: 0, max_failures: '', use_detect: true }, when: '',
};

let DEP = { computers: [], packages: [], groups: [] };

async function pageDeploy(el) {
  const [{ packages }, computers, g] = await Promise.all([loadPackages(true), GET('/computers'), GET('/groups')]);
  DEP = { computers, packages, groups: g.groups };
  el.innerHTML = `
    <div class="page-head"><h1>New deployment</h1></div>
    <div class="card"><div class="card-head"><span class="step">1</span><h2>What to deploy</h2>
      <div class="right tabs">
        <button class="${W.mode === 'package' ? 'on' : ''}" onclick="App.wMode('package')">Package</button>
        <button class="${W.mode === 'command' ? 'on' : ''}" onclick="App.wMode('command')">Run a command</button>
      </div></div><div class="card-body" id="w-what"></div></div>
    <div class="card"><div class="card-head"><span class="step">2</span><h2>Where</h2><div class="right small muted" id="w-count"></div></div>
      <div class="card-body" id="w-where"></div></div>
    <div class="card"><div class="card-head"><span class="step">3</span><h2>Settings</h2></div><div class="card-body" id="w-opts"></div></div>
    <div class="card"><div class="summary-bar"><div class="what" id="w-summary"></div>
      <button class="primary big" id="w-go" onclick="App.wDeploy()">Deploy</button></div></div>`;
  renderWhat();
  renderWhere();
  renderOpts();
}

function selectedPackage() { return DEP.packages.find((p) => p.id === W.pkg); }

function renderWhat() {
  const box = $('#w-what');
  if (W.mode === 'command') {
    box.innerHTML = `
      <div class="row"><div class="field" style="max-width:220px"><label>Shell</label><select onchange="App.wSet('shell', this.value)">
        ${[['', 'Default (PowerShell / bash)'], ['powershell', 'PowerShell (Windows)'], ['cmd', 'cmd (Windows)'], ['bash', 'bash (Linux)'], ['sh', 'sh (Linux)']]
          .map(([v, l]) => `<option value="${v}" ${W.shell === v ? 'selected' : ''}>${l}</option>`).join('')}</select></div></div>
      <div class="field"><label>Command or script</label>
        <textarea class="code" style="min-height:140px" spellcheck="false" placeholder="Get-Service W3SVC&#10;# or on Linux: systemctl status haproxy"
          oninput="App.wSet('command', this.value)">${esc(W.command)}</textarea>
        <div class="hint">Runs as administrator (Windows) or via sudo (Linux). Output is shown per computer.</div></div>`;
  } else {
    const q = W.q.toLowerCase();
    const list = DEP.packages.filter((p) => !q || (p.name + ' ' + p.description + ' ' + p.category).toLowerCase().includes(q));
    box.innerHTML = `<div class="field"><input class="search" placeholder="Search packages…" value="${esc(W.q)}" oninput="App.wSearch(this.value)" id="w-q"></div>
      ${groupByCategory(list).map(([cat, ps]) => `<div class="cat">${esc(cat)}</div><div class="pkg-grid">
        ${ps.map((p) => `<div class="pkg ${W.pkg === p.id ? 'on' : ''}" onclick="App.wPick(${jstr(p.id)})">
          <div class="t">${esc(p.name)}</div><div class="d">${esc(p.description)}</div><div class="m">${platformText(p)}</div></div>`).join('')}
      </div>`).join('') || '<div class="muted">No packages match.</div>'}`;
  }
  renderSummary();
}

function renderWhere() {
  const tags = [...new Set(DEP.computers.flatMap((c) => c.tags))].sort();
  const q = W.cq.toLowerCase();
  const list = DEP.computers.filter((c) => (!W.cos || c.os === W.cos) && (!q || (c.name + ' ' + c.tags.join(' ') + ' ' + (c.ad_ou || '')).toLowerCase().includes(q)));
  $('#w-where').innerHTML = `
    ${DEP.groups.length ? `<div class="field"><label>Groups</label><div class="chips">
      ${DEP.groups.map((g) => `<span class="chip ${W.groups.has(g.name) ? 'on' : ''}" onclick="App.wGroup(${jstr(g.name)})">${g.kind === 'dynamic' ? '⚡ ' : ''}${esc(g.name)} <span class="small">${g.members.length}</span></span>`).join('')}
    </div></div>` : ''}
    ${tags.length ? `<div class="field"><label>Tags <span class="muted small">(all computers with the tag, resolved when the deployment starts)</span></label><div class="chips">
      ${tags.map((t) => `<span class="chip ${W.tags.has(t) ? 'on' : ''}" onclick="App.wTag(${jstr(t)})">${esc(t)} <span class="small">${DEP.computers.filter((c) => c.tags.includes(t)).length}</span></span>`).join('')}
    </div></div>` : ''}
    <div class="field"><label>Computers</label>
      <div class="row" style="margin-bottom:8px"><input class="search" placeholder="Filter…" value="${esc(W.cq)}" oninput="App.wCq(this.value)" id="w-cq">
        <select style="width:auto" onchange="App.wSet('cos', this.value); renderWhere()">
          <option value="">All OS</option><option value="windows" ${W.cos === 'windows' ? 'selected' : ''}>Windows</option><option value="linux" ${W.cos === 'linux' ? 'selected' : ''}>Linux</option></select>
        <button onclick="App.wAll(true)">Select shown</button><button onclick="App.wAll(false)">Clear</button></div>
      ${DEP.computers.length ? `<div class="table-wrap" style="max-height:300px;overflow:auto;border:1px solid var(--border);border-radius:8px"><table><tbody>
        ${list.map((c) => `<tr class="click ${W.computers.has(c.name) ? 'sel' : ''}" onclick="App.wComp(${jstr(c.name)})">
          <td style="width:30px"><input type="checkbox" ${W.computers.has(c.name) ? 'checked' : ''} onclick="event.preventDefault()"></td>
          <td><b>${esc(c.name)}</b></td><td class="small">${osLabel(c.os)}</td>
          <td class="small"><span class="dot ${c.online === true ? 'on' : c.online === false ? 'off' : ''}"></span></td><td>${tagsHtml(c.tags)}</td></tr>`).join('')}
      </tbody></table></div>` : '<div class="muted small">No computers in the list yet — type names below or <a href="#/computers">add computers</a>.</div>'}
    </div>
    <div class="field"><label>Other computers <span class="muted small">(not in the list, comma or line separated)</span></label>
      <textarea rows="2" oninput="App.wSet('extra', this.value)" placeholder="server07.corp.local">${esc(W.extra)}</textarea></div>`;
  renderSummary();
}

function renderOpts() {
  const p = selectedPackage();
  const vars = W.mode === 'package' && p ? Object.entries(p.variables || {}) : [];
  $('#w-opts').innerHTML = `
    ${vars.length ? `<div class="cat">Package variables</div><div class="row">
      ${vars.map(([k, v]) => {
        const val = W.vars[k] ?? v.default ?? '';
        const input = v.options && v.options.length
          ? `<select onchange="App.wVar(${jstr(k)}, this.value)">${v.options.map((o) => `<option ${o === val ? 'selected' : ''}>${esc(o)}</option>`).join('')}</select>`
          : `<input type="${v.secret ? 'password' : 'text'}" value="${esc(val)}" oninput="App.wVar(${jstr(k)}, this.value)" ${v.required ? 'required' : ''}>`;
        return `<div class="field" style="min-width:240px"><label>${esc(k)}${v.required ? ' <span class="err">*</span>' : ''}</label>${input}
          ${v.description ? `<div class="hint">${esc(v.description)}</div>` : ''}</div>`;
      }).join('')}</div>` : ''}
    <div class="cat">How to run</div>
    <div class="row">
      <div class="field"><label>Parallel computers</label><input type="number" min="1" max="256" value="${W.opt.concurrency}" oninput="App.wOpt('concurrency', +this.value || 1)">
        <div class="hint">How many computers are worked on at once.</div></div>
      <div class="field"><label>Batch size (rolling)</label><input type="number" min="0" value="${W.opt.batch_size}" oninput="App.wOpt('batch_size', +this.value || 0)">
        <div class="hint">0 = all at once. 1 = one at a time (HA pairs, web farms).</div></div>
      <div class="field"><label>Stop after failures</label><input type="number" min="0" value="${W.opt.max_failures}" placeholder="never" oninput="App.wOpt('max_failures', this.value)">
        <div class="hint">Don't start more computers once this many failed. 0 = stop at first failure.</div></div>
      <div class="field"><label>Start at</label><input type="datetime-local" value="${esc(W.when)}" oninput="App.wSet('when', this.value)">
        <div class="hint">Empty = now.</div></div>
    </div>
    ${W.mode === 'package' && p && p.has_detect ? `<label class="check"><input type="checkbox" ${W.opt.use_detect ? 'checked' : ''} onchange="App.wOpt('use_detect', this.checked)">
      Skip computers that already have it (run the package's detect check)</label>` : ''}`;
  renderSummary();
}

function extraNames() { return W.extra.split(/[\s,;]+/).map((s) => s.trim()).filter(Boolean); }

function targetCount() {
  const names = new Set([...W.computers].map((n) => n.toLowerCase()));
  DEP.groups.filter((g) => W.groups.has(g.name)).forEach((g) => g.members.forEach((m) => names.add(m.toLowerCase())));
  DEP.computers.forEach((c) => { if (c.tags.some((t) => W.tags.has(t))) names.add(c.name.toLowerCase()); });
  extraNames().forEach((n) => names.add(n.toLowerCase()));
  return names.size;
}

function renderSummary() {
  const s = $('#w-summary');
  if (!s) return;
  const n = targetCount();
  const p = selectedPackage();
  const what = W.mode === 'command' ? (W.command.trim() ? 'a command' : null) : p ? `<b>${esc(p.name)}</b>` : null;
  $('#w-count').textContent = n ? `${n} computer${n === 1 ? '' : 's'} selected` : '';
  s.innerHTML = !what ? '<span class="muted">1. Choose a package or enter a command.</span>'
    : !n ? `Deploy ${what} — <span class="muted">2. choose computers.</span>`
      : `Deploy ${what} to <b>${n}</b> computer${n === 1 ? '' : 's'}${W.opt.batch_size ? `, ${W.opt.batch_size} at a time` : ''}${W.when ? `, starting ${esc(W.when.replace('T', ' '))}` : ''}.`;
  $('#w-go').disabled = !what || !n;
  $('#w-go').textContent = W.when ? 'Schedule' : 'Deploy';
}

Object.assign(App, {
  wMode(m) { W.mode = m; pageDeploy($('#main')); },
  wSet(k, v) { W[k] = v; renderSummary(); },
  wSearch(v) { W.q = v; renderWhat(); const i = $('#w-q'); i.focus(); i.setSelectionRange(v.length, v.length); },
  wPick(id) { if (W.pkg !== id) W.vars = {}; W.pkg = id; renderWhat(); renderOpts(); },
  wTag(t) { W.tags.has(t) ? W.tags.delete(t) : W.tags.add(t); renderWhere(); },
  wGroup(g) { W.groups.has(g) ? W.groups.delete(g) : W.groups.add(g); renderWhere(); },
  wComp(n) { W.computers.has(n) ? W.computers.delete(n) : W.computers.add(n); renderWhere(); },
  wCq(v) { W.cq = v; renderWhere(); const i = $('#w-cq'); i.focus(); i.setSelectionRange(v.length, v.length); },
  wAll(on) {
    const q = W.cq.toLowerCase();
    DEP.computers.filter((c) => (!W.cos || c.os === W.cos) && (!q || (c.name + ' ' + c.tags.join(' ') + ' ' + (c.ad_ou || '')).toLowerCase().includes(q)))
      .forEach((c) => (on ? W.computers.add(c.name) : W.computers.delete(c.name)));
    if (!on) { W.tags.clear(); W.groups.clear(); }
    renderWhere();
  },
  wVar(k, v) { W.vars[k] = v; },
  wOpt(k, v) { W.opt[k] = v; renderSummary(); },
  wDeploy: act(async () => {
    const p = selectedPackage();
    const variables = {};
    if (W.mode === 'package' && p) {
      Object.entries(p.variables || {}).forEach(([k, v]) => {
        const val = W.vars[k] ?? v.default;
        if (val !== undefined && val !== null && val !== v.default) variables[k] = String(val);
        if (v.required && !String(val ?? '').trim()) throw new Error(`Fill in the variable '${k}'`);
      });
    }
    const body = {
      targets: { computers: [...W.computers, ...extraNames()], tags: [...W.tags], groups: [...W.groups] },
      options: {
        concurrency: W.opt.concurrency, batch_size: W.opt.batch_size, use_detect: W.opt.use_detect,
        max_failures: W.opt.max_failures === '' ? null : +W.opt.max_failures,
      },
      variables,
      scheduled_for: W.when ? W.when.replace('T', ' ') : null,
    };
    if (W.mode === 'command') { body.command = W.command; body.shell = W.shell || null; } else { body.package_id = W.pkg; }
    $('#w-go').disabled = true;
    const r = await POST('/deployments', body);
    W.when = '';
    location.hash = '#/deployment/' + r.id;
  }),
});

// ------------------------------------------------------------- deployments

async function pageHistory(el) {
  const list = await GET('/deployments?limit=200');
  el.innerHTML = `<div class="page-head"><h1>Deployments</h1>
    <div class="actions"><button class="primary" onclick="location.hash='#/deploy'">New deployment</button></div></div>
    <div class="card"><div class="card-body flush">${deploymentTable(list)}</div></div>`;
  if (list.some((d) => ['running', 'queued', 'scheduled'].includes(d.status))) poll(() => pageHistory(el).catch(() => {}), 4000);
}

let DSEL = null; // selected target id in the detail page

async function pageDeployment(el, id) {
  const { deployment: d, targets, package_yaml } = await GET('/deployments/' + id);
  if (!DSEL || !targets.some((t) => t.id === DSEL)) {
    DSEL = (targets.find((t) => t.status === 'running') || targets.find((t) => ['failed', 'offline'].includes(t.status)) || targets[0] || {}).id;
  }
  const running = ['running', 'queued'].includes(d.status);
  const opts = d.options;
  const varList = Object.entries(d.variables || {});
  el.innerHTML = `
    <div class="page-head"><h1>${esc(d.package_name)}</h1>${pill(d.status)}
      <span class="sub">${fmtTime(d.started_at || d.created_at)}${d.finished_at ? ' · took ' + elapsed(d.started_at, d.finished_at) : running && d.started_at ? ' · running ' + elapsed(d.started_at) : ''}</span>
      <div class="actions">
        ${running || d.status === 'scheduled' ? `<button class="danger" onclick="App.dCancel('${d.id}')">Cancel</button>` : ''}
        ${!running && d.n_failed ? `<button onclick="App.dRedeploy('${d.id}', true)">Redeploy to failed (${d.n_failed})</button>` : ''}
        ${!running ? `<button onclick="App.dRedeploy('${d.id}', false)">Redeploy</button>` : ''}
        ${!running ? `<button class="danger" onclick="App.dDelete('${d.id}')">Delete</button>` : ''}
      </div></div>
    <div class="stats">
      <div class="stat"><div class="n">${targets.length}</div><div class="l">Computers</div></div>
      <div class="stat"><div class="n" style="color:var(--ok)">${d.n_success}</div><div class="l">Succeeded</div></div>
      <div class="stat"><div class="n">${d.n_skipped}</div><div class="l">Skipped (already OK)</div></div>
      <div class="stat"><div class="n" style="color:${d.n_failed ? 'var(--bad)' : 'inherit'}">${d.n_failed}</div><div class="l">Failed / offline</div></div>
    </div>
    ${d.status === 'scheduled' ? `<div class="card"><div class="card-body">Scheduled for <b>${fmtTime(d.scheduled_for)}</b> (${ago(d.scheduled_for)}).</div></div>` : ''}
    <div class="split">
      <div>
        <div class="card"><div class="card-head"><h2>Computers</h2></div><div class="card-body flush"><div class="table-wrap"><table>
          <thead><tr><th>Computer</th><th>Status</th><th>Step / result</th><th>Time</th></tr></thead><tbody>
          ${targets.map((t) => `<tr class="click ${t.id === DSEL ? 'sel' : ''}" onclick="App.dSel(${t.id})">
            <td><b>${esc(t.computer)}</b><div class="os">${osLabel(t.os)}</div></td><td>${pill(t.status)}</td>
            <td class="small">${t.status === 'running' ? esc(t.current_step || '') : `<span class="${['failed', 'offline'].includes(t.status) ? 'err' : 'muted'}">${esc(t.error || '')}</span>`}</td>
            <td class="small muted num">${elapsed(t.started_at, t.finished_at)}</td></tr>`).join('') || '<tr><td colspan="4" class="muted">Waiting to start…</td></tr>'}
          </tbody></table></div></div></div>
        <div class="card"><div class="card-head"><h2>Details</h2></div><div class="card-body small">
          <div>Started by: <b>${esc(d.source)}</b> · ID <code>${d.id.slice(0, 8)}</code></div>
          <div>Targets: ${(d.targets.groups || []).map((g) => `<span class="tag on">${esc(g)}</span>`).join('')}${d.targets.tags.map((t) => `<span class="tag">${esc(t)}</span>`).join('')} ${esc(d.targets.computers.join(', '))}</div>
          <div>Parallel: ${opts.concurrency} · Batch: ${opts.batch_size || 'all'} · Stop after failures: ${opts.max_failures ?? 'never'} · Detect: ${opts.use_detect ? 'on' : 'off'}</div>
          ${varList.length ? `<div>Variables: ${varList.map(([k, v]) => `<code>${esc(k)}=${esc(v)}</code>`).join(' ')}</div>` : ''}
          <details style="margin-top:8px"><summary>Package definition used</summary><pre class="log" style="min-height:0;border-radius:8px;margin-top:6px">${esc(package_yaml)}</pre></details>
        </div></div>
      </div>
      <div class="card"><div class="card-head"><h2 id="log-title">Log</h2><div class="right"><button onclick="App.dCopyLog()">Copy</button></div></div>
        <pre class="log" id="log"></pre></div>
    </div>`;
  await loadLog(targets);
  if (running || targets.some((t) => ['running', 'pending'].includes(t.status))) {
    poll(() => pageDeployment(el, id).catch(() => {}), 2000);
  } else if (d.status === 'scheduled') {
    poll(() => pageDeployment(el, id).catch(() => {}), 10000);
  }
}

async function loadLog(targets) {
  const t = targets.find((x) => x.id === DSEL);
  const box = $('#log');
  if (!t) { box.textContent = ''; return; }
  $('#log-title').textContent = 'Log — ' + t.computer;
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  const text = await GET(`/targets/${t.id}/log`);
  box.innerHTML = esc(text || (t.status === 'pending' ? 'Waiting…' : '')).split('\n').map((l) =>
    /==&gt; (SUCCESS|SKIPPED)/.test(l) ? `<span class="k">${l}</span>`
      : /==&gt; (FAILED|OFFLINE|CANCELLED)|ERROR:|failed:/.test(l) ? `<span class="e">${l}</span>`
        : /==&gt;/.test(l) ? `<span class="h">${l}</span>` : l).join('\n');
  if (atBottom || t.status !== 'running') box.scrollTop = box.scrollHeight;
}

Object.assign(App, {
  dSel(id) { DSEL = id; route(); },
  dCancel: act(async (id) => { await POST(`/deployments/${id}/cancel`); toast('Cancelling — running steps finish first'); route(); }),
  dRedeploy: act(async (id, onlyFailed) => {
    const r = await POST(`/deployments/${id}/redeploy`, { only_failed: onlyFailed });
    DSEL = null;
    location.hash = '#/deployment/' + r.id;
  }),
  dDelete: act(async (id) => {
    if (!confirm('Delete this deployment and its logs?')) return;
    await DEL('/deployments/' + id);
    location.hash = '#/history';
  }),
  dCopyLog() { navigator.clipboard.writeText($('#log').textContent).then(() => toast('Copied')); },
});

// --------------------------------------------------------------- schedules

const DAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
const WEEKS = [[1, '1st'], [2, '2nd'], [3, '3rd'], [4, '4th'], [5, 'last']];

/** "2nd Tue of the month + 2 days" (2nd Tuesday is Patch Tuesday). */
function monthlyText(s) {
  const day = `${(WEEKS.find(([w]) => w === s.week) || [0, '?'])[1]} ${DAYS[s.weekdays[0]] || '?'}`;
  const base = s.week === 2 && s.weekdays[0] === 1 ? 'Patch Tuesday' : `${day} of the month`;
  return s.offset_days ? `${base} ${s.offset_days > 0 ? '+' : '−'} ${Math.abs(s.offset_days)} day${Math.abs(s.offset_days) === 1 ? '' : 's'}` : base;
}

async function pageSchedules(el) {
  const [list, { packages }] = await Promise.all([GET('/schedules'), loadPackages(true)]);
  const pname = (id) => (packages.find((p) => p.id === id) || { name: id }).name;
  const when = (s) => s.kind === 'once' ? 'once at ' + s.time : s.kind === 'daily' ? 'daily at ' + s.time
    : s.kind === 'weekly' ? `${s.weekdays.map((d) => DAYS[d]).join(', ')} at ${s.time}`
      : s.kind === 'monthly' ? `${monthlyText(s)} at ${s.time}` : `every ${s.interval_minutes} min`;
  el.innerHTML = `<div class="page-head"><h1>Schedules</h1><span class="sub">Recurring deployments (patch nights, compliance re-runs)</span>
    <div class="actions"><button class="primary" onclick="App.editSchedule()">New schedule</button></div></div>
    <div class="card"><div class="card-body flush">${list.length ? `<table><thead><tr><th>Name</th><th>Package</th><th>When</th><th>Targets</th><th>Next run</th><th>Last run</th><th></th></tr></thead><tbody>
      ${list.map((s) => `<tr><td><b>${esc(s.name)}</b>${s.enabled ? '' : ' <span class="tag">disabled</span>'}</td><td>${esc(pname(s.package_id))}</td>
        <td class="small">${esc(when(s))}</td>
        <td class="small">${(s.targets.groups || []).map((g) => `<span class="tag on">${esc(g)}</span>`).join('')}${tagsHtml(s.targets.tags)} ${esc(s.targets.computers.join(', '))}</td>
        <td class="small">${s.next_run ? fmtTime(s.next_run) : '-'}</td><td class="small">${s.last_run ? ago(s.last_run) : 'never'}</td>
        <td style="white-space:nowrap"><button onclick='App.editSchedule(${JSON.stringify(s.id)})'>Edit</button>
          <button class="danger" onclick="App.delSchedule(${s.id})">Delete</button></td></tr>`).join('')}
      </tbody></table>` : '<div class="empty"><b>No schedules</b>Run a package automatically, e.g. every Saturday at 02:00 for the "web" tag.</div>'}</div></div>`;
  App._schedules = list;
}

Object.assign(App, {
  async editSchedule(id, preset = {}) {
    const { packages } = await loadPackages();
    const [computers, gr] = await Promise.all([GET('/computers'), GET('/groups')]);
    const groupNames = gr.groups.map((g) => g.name);
    const tags = [...new Set(computers.flatMap((c) => c.tags))].sort();
    const s = (id && (App._schedules || []).find((x) => x.id === id)) || {
      id: 0, name: '', package_id: packages[0] && packages[0].id, kind: 'weekly', time: '02:00', weekdays: [5], week: 2, offset_days: 0,
      interval_minutes: 60, enabled: true, targets: { computers: [], tags: [], groups: [] }, variables: {}, options: { concurrency: 10, batch_size: 0, max_failures: null, use_detect: true },
      ...preset,
    };
    const m = modal(id ? 'Edit schedule' : 'New schedule', `
      <div class="row"><div class="field"><label>Name</label><input id="s-name" value="${esc(s.name)}" placeholder="Saturday patching"></div>
        <div class="field"><label>Package</label><select id="s-pkg">${packages.map((p) => `<option value="${esc(p.id)}" ${p.id === s.package_id ? 'selected' : ''}>${esc(p.name)}</option>`).join('')}</select></div></div>
      <div class="row"><div class="field"><label>Repeat</label><select id="s-kind">
          ${[['once', 'Once'], ['daily', 'Daily'], ['weekly', 'Weekly'], ['monthly', 'Monthly (e.g. Patch Tuesday)'], ['interval', 'Every N minutes']].map(([v, l]) => `<option value="${v}" ${s.kind === v ? 'selected' : ''}>${l}</option>`).join('')}</select></div>
        <div class="field" id="s-time-f"><label>Time</label><input id="s-time" value="${esc(s.time)}" placeholder="02:00"></div>
        <div class="field" id="s-int-f"><label>Minutes</label><input id="s-int" type="number" min="1" value="${s.interval_minutes}"></div></div>
      <div class="row" id="s-month-f">
        <div class="field"><label>Week</label><select id="s-week">${WEEKS.map(([w, l]) => `<option value="${w}" ${s.week === w ? 'selected' : ''}>${l}</option>`).join('')}</select></div>
        <div class="field"><label>Weekday</label><select id="s-wd">${DAYS.map((d, i) => `<option value="${i}" ${s.weekdays[0] === i ? 'selected' : ''}>${d}</option>`).join('')}</select></div>
        <div class="field"><label>Days after</label><input id="s-off" type="number" min="-27" max="27" value="${s.offset_days || 0}">
          <div class="hint">2nd Tue + 2 = the Thursday after Patch Tuesday.</div></div></div>
      <div class="field" id="s-days-f"><label>Days</label><div class="chips">${DAYS.map((d, i) => `<label class="chip ${s.weekdays.includes(i) ? 'on' : ''}"><input type="checkbox" data-day="${i}" ${s.weekdays.includes(i) ? 'checked' : ''} style="display:none">${d}</label>`).join('')}</div></div>
      <div class="field"><label>Groups</label><div class="chips">${groupNames.map((t) => `<label class="chip ${(s.targets.groups || []).includes(t) ? 'on' : ''}"><input type="checkbox" data-group="${esc(t)}" ${(s.targets.groups || []).includes(t) ? 'checked' : ''} style="display:none">${esc(t)}</label>`).join('') || '<span class="muted small">No groups yet</span>'}</div></div>
      <div class="field"><label>Tags</label><div class="chips">${tags.map((t) => `<label class="chip ${s.targets.tags.includes(t) ? 'on' : ''}"><input type="checkbox" data-tag="${esc(t)}" ${s.targets.tags.includes(t) ? 'checked' : ''} style="display:none">${esc(t)}</label>`).join('') || '<span class="muted small">No tags yet</span>'}</div></div>
      <div class="field"><label>Computers</label><textarea id="s-comps" rows="2" placeholder="comma separated">${esc(s.targets.computers.join(', '))}</textarea></div>
      <div id="s-vars"></div>
      <div class="row"><div class="field"><label>Batch size</label><input id="s-batch" type="number" min="0" value="${s.options.batch_size}"></div>
        <div class="field"><label>Parallel</label><input id="s-conc" type="number" min="1" value="${s.options.concurrency}"></div>
        <div class="field"><label class="check"><input type="checkbox" id="s-en" ${s.enabled ? 'checked' : ''}> Enabled</label></div></div>`,
    [{ label: 'Cancel' }, {
      label: 'Save', cls: 'primary', onClick: async (root) => {
        const variables = {};
        $$('[data-var]', root).forEach((i) => { if (i.value !== i.dataset.default) variables[i.dataset.var] = i.value; });
        const body = {
          ...s, name: $('#s-name').value.trim(), package_id: $('#s-pkg').value, kind: $('#s-kind').value, time: $('#s-time').value.trim(),
          interval_minutes: +$('#s-int').value || 0, enabled: $('#s-en').checked,
          weekdays: $('#s-kind').value === 'monthly' ? [+$('#s-wd').value] : $$('[data-day]', root).filter((c) => c.checked).map((c) => +c.dataset.day),
          week: +$('#s-week').value, offset_days: +$('#s-off').value || 0,
          targets: { groups: $$('[data-group]', root).filter((c) => c.checked).map((c) => c.dataset.group), tags: $$('[data-tag]', root).filter((c) => c.checked).map((c) => c.dataset.tag), computers: $('#s-comps').value.split(/[\s,;]+/).filter(Boolean) },
          options: { ...s.options, batch_size: +$('#s-batch').value || 0, concurrency: +$('#s-conc').value || 10 },
          variables,
        };
        const r = await POST('/schedules', body);
        toast('Saved — next run ' + fmtTime(r.next_run));
        route();
      },
    }]);
    const root = m.root;
    $$('.chip input', root).forEach((cb) => { cb.onchange = () => cb.parentElement.classList.toggle('on', cb.checked); });
    const kindVis = () => {
      const k = $('#s-kind').value;
      $('#s-days-f').style.display = k === 'weekly' ? '' : 'none';
      $('#s-month-f').style.display = k === 'monthly' ? '' : 'none';
      $('#s-int-f').style.display = k === 'interval' ? '' : 'none';
      $('#s-time-f').style.display = k === 'interval' ? 'none' : '';
      $('#s-time').placeholder = k === 'once' ? '2026-11-07 02:00' : '02:00';
    };
    const varsVis = () => {
      const p = packages.find((x) => x.id === $('#s-pkg').value);
      const vars = Object.entries((p && p.variables) || {});
      $('#s-vars').innerHTML = vars.length ? '<div class="cat">Variables</div><div class="row">' + vars.map(([k, v]) => {
        const val = (p.id === s.package_id && s.variables[k]) ?? v.default ?? '';
        return `<div class="field"><label>${esc(k)}${v.required ? ' <span class="err">*</span>' : ''}</label>
          <input data-var="${esc(k)}" data-default="${esc(v.default ?? '')}" value="${esc(val)}" type="${v.secret ? 'password' : 'text'}"></div>`;
      }).join('') + '</div>' : '';
    };
    $('#s-kind').onchange = kindVis;
    $('#s-pkg').onchange = varsVis;
    kindVis();
    varsVis();
  },
  delSchedule: act(async (id) => {
    if (!confirm('Delete this schedule?')) return;
    await DEL('/schedules/' + id);
    route();
  }),
});

// ---------------------------------------------------------------- settings

async function pageSettings(el) {
  const [s, creds, upd] = await Promise.all([GET('/settings'), GET('/credentials'), GET('/update')]);
  CS.creds = creds;
  const ad = s.ad;
  el.innerHTML = `<div class="page-head"><h1>Settings</h1><span class="sub">v${esc(s.version)}</span></div>
    <div class="card"><div class="card-head"><h2>Credentials</h2><span class="small muted">Passwords are encrypted with Windows DPAPI for the account running PSCCM</span>
      <div class="right"><button class="primary" onclick="App.addCred()">Add credential</button></div></div>
      <div class="card-body flush">${creds.length ? `<table><thead><tr><th>Name</th><th>Type</th><th>User</th><th>Default for</th><th></th></tr></thead><tbody>
        ${creds.map((c) => `<tr><td><b>${esc(c.name)}</b></td><td>${esc({ windows: 'Windows (WinRM)', ssh: 'Linux (SSH)', ldap: 'Active Directory (LDAP)' }[c.kind] || c.kind)}</td>
          <td>${c.username ? esc(c.username) : '<span class="muted">account running PSCCM</span>'}${c.key_path ? `<div class="small muted">key ${esc(c.key_path)}</div>` : ''}</td>
          <td>${c.default_for ? `<span class="tag on">${esc(c.default_for)}</span>` : ''}</td>
          <td><button class="danger" onclick="App.delCred(${c.id})">Delete</button></td></tr>`).join('')}</tbody></table>`
        : '<div class="empty"><b>No credentials</b>Windows computers work without one when PSCCM runs as a domain account with admin rights.</div>'}</div></div>

    <div class="card"><div class="card-head"><h2>Active Directory</h2><span class="small muted">Read-only: PSCCM only searches computer objects, it never changes AD</span></div>
      <div class="card-body">
        <div class="row">
          <div class="field"><label>Connect as</label><select id="ad-mode">
            <option value="windows" ${ad.mode === 'windows' ? 'selected' : ''}>Account running PSCCM (Windows, no password)</option>
            <option value="ldap" ${ad.mode === 'ldap' ? 'selected' : ''}>LDAP with a stored credential</option></select></div>
          <div class="field"><label>Domain controller / domain</label><input id="ad-server" value="${esc(ad.server)}" placeholder="corp.local or dc01.corp.local">
            <div class="hint">Optional in Windows mode.</div></div>
        </div>
        <div class="row" id="ad-ldap">
          <div class="field"><label>Security</label><select id="ad-sec">
            ${[['ldaps', 'LDAPS (636) — recommended'], ['starttls', 'StartTLS (389)'], ['none', 'None (389, plain text)']].map(([v, l]) => `<option value="${v}" ${ad.security === v ? 'selected' : ''}>${l}</option>`).join('')}</select></div>
          <div class="field"><label>Credential (type LDAP)</label><select id="ad-cred">${credOptions(ad.credential_id, ['ldap'], 'Choose…')}</select>
            <div class="hint">Any normal domain user works — no extra rights needed.</div></div>
          <div class="field"><label class="check"><input type="checkbox" id="ad-skip" ${ad.skip_tls_verify ? 'checked' : ''}> Don't verify the DC certificate</label></div>
        </div>
        <div class="field"><label>Search in OUs</label><textarea id="ad-bases" rows="2" placeholder="OU=Servers,DC=corp,DC=local">${esc(ad.base_dns.join('\n'))}</textarea>
          <div class="hint">One distinguished name per line. Empty = the whole domain.</div></div>
        <div class="row">
          <div class="field"><label>Extra LDAP filter</label><input id="ad-filter" value="${esc(ad.filter)}" placeholder="(operatingSystem=*Server*)"></div>
          <div class="field"><label>Skip computers not seen for (days)</label><input id="ad-stale" type="number" min="0" value="${ad.stale_days}"><div class="hint">0 = keep all.</div></div>
          <div class="field"><label>Auto-sync every (minutes)</label><input id="ad-auto" type="number" min="0" value="${ad.auto_sync_minutes}"><div class="hint">0 = only when you click Import.</div></div>
        </div>
        <label class="check"><input type="checkbox" id="ad-disabled" ${ad.include_disabled ? 'checked' : ''}> Include disabled computer accounts</label>
        <label class="check"><input type="checkbox" id="ad-tag" ${ad.tag_with_ou ? 'checked' : ''}> Tag computers with their OU name (e.g. OU=Web Servers → web-servers)</label>
        <div style="margin-top:14px;display:flex;gap:8px;align-items:center"><button class="primary" onclick="App.saveAd()">Save</button>
          <button onclick="App.testAd()">Test</button><span id="ad-result" class="small"></span>
          ${ad.last_sync ? `<span class="small muted" style="margin-left:auto">Last import ${ago(ad.last_sync)}</span>` : ''}</div>
      </div></div>

    <div class="grid two">
      <div class="card"><div class="card-head"><h2>Windows (WinRM)</h2></div><div class="card-body">
        <label class="check"><input type="checkbox" id="w-ssl" ${s.winrm.use_ssl ? 'checked' : ''}> Use HTTPS (port 5986)</label>
        <label class="check"><input type="checkbox" id="w-skip" ${s.winrm.skip_ca_check ? 'checked' : ''}> Accept self-signed WinRM certificates</label>
        <p class="small muted">HTTP on 5985 is already encrypted by Kerberos/NTLM in a domain. Targets need WinRM enabled
          (<code>Enable-PSRemoting</code> or the GPO "Allow remote server management through WinRM").</p>
        <button onclick="App.saveWinrm()">Save</button></div></div>
      <div class="card"><div class="card-head"><h2>Linux (SSH) host keys</h2></div><div class="card-body">
        <p class="small muted">Host keys are remembered the first time PSCCM connects. If a server is rebuilt, forget its key.</p>
        <div class="row"><div class="field"><input id="kh-host" placeholder="web01.corp.local or host:port"></div><button onclick="App.forgetHost()">Forget key</button></div></div></div>
    </div>
    <div class="card"><div class="card-head"><h2>Agents</h2>
      <span class="small muted">For computers with no WinRM/SSH — they connect out to this console</span></div>
      <div class="card-body">
        <label class="check"><input type="checkbox" id="ag-enabled" ${s.agent.enabled ? 'checked' : ''}> Allow new agents to enroll</label>
        ${s.agent.enroll_token ? `
          <div class="field" style="margin-top:10px"><label>Enrollment token</label>
            <div class="row"><input id="ag-token" readonly value="${esc(s.agent.enroll_token)}" style="font-family:monospace">
              <button onclick="App.agRotate()">Regenerate</button></div>
            <div class="hint">Regenerating does not disconnect existing agents.</div></div>
          <div class="field"><label>Install on a computer (run as administrator, PowerShell)</label>
            <pre class="log" id="ag-cmd" style="min-height:0;white-space:pre-wrap">${esc(agentInstallCmd(s.agent.enroll_token))}</pre>
            <div style="display:flex;gap:8px;margin-top:6px"><button onclick="App.agCopy()">Copy command</button>
              <span class="small muted">Or push it with a GPO startup/login script, or deploy the <b>Install PSCCM Agent</b> package to machines that still have WinRM/SSH.</span></div></div>
          <p class="small muted">The agent runs as LocalSystem and can run commands, read event logs and manage the firewall, tasks and local
            users on that computer. Anyone who can use this console controls every enrolled machine — protect it like a domain admin account.</p>`
          : `<div style="margin-top:10px"><button class="primary" onclick="App.agRotate()">Generate enrollment token</button>
            <span class="small muted">Creates the token agents use to join.</span></div>`}
        <div style="margin-top:10px"><button onclick="App.saveAgent()">Save</button></div>
      </div></div>
    ${updateCard(upd)}
    <div class="card"><div class="card-head"><h2>Folders</h2></div><div class="card-body small">
      <div>Data: <code>${esc(s.data_dir)}</code></div><div>Packages: <code>${esc(s.packages_dir)}</code></div><div>Repository: <code>${esc(s.repo_dir)}</code></div></div></div>`;
  const vis = () => { $('#ad-ldap').style.display = $('#ad-mode').value === 'ldap' ? '' : 'none'; };
  $('#ad-mode').onchange = vis;
  vis();
}

/// The PowerShell one-liner that downloads and installs the agent. Built from
/// the console's own origin so no server URL needs configuring.
function agentInstallCmd(token) {
  const u = location.origin;
  return `[Net.ServicePointManager]::ServerCertificateValidationCallback={$true}; `
    + `$u='${u}'; $t='${token}'; $d='C:\\Program Files\\PSCCM-Agent'; `
    + `New-Item -Type Directory -Force $d | Out-Null; `
    + `Invoke-WebRequest "$u/download/agent?token=$t" -OutFile "$d\\psccm.exe" -UseBasicParsing; `
    + `& "$d\\psccm.exe" agent install --server $u --token $t`;
}

function adForm() {
  return {
    mode: $('#ad-mode').value, server: $('#ad-server').value.trim(), security: $('#ad-sec').value,
    skip_tls_verify: $('#ad-skip').checked, credential_id: +$('#ad-cred').value || null,
    base_dns: $('#ad-bases').value.split('\n').map((s) => s.trim()).filter(Boolean),
    filter: $('#ad-filter').value.trim(), stale_days: +$('#ad-stale').value || 0,
    auto_sync_minutes: +$('#ad-auto').value || 0, include_disabled: $('#ad-disabled').checked, tag_with_ou: $('#ad-tag').checked,
  };
}

Object.assign(App, {
  addCred() {
    modal('Add credential', `
      <div class="row"><div class="field"><label>Name</label><input id="cr-name" placeholder="domain-admin"></div>
        <div class="field"><label>Type</label><select id="cr-kind">
          <option value="windows">Windows computers (WinRM)</option><option value="ssh">Linux computers (SSH)</option><option value="ldap">Active Directory (LDAP read-only)</option></select></div></div>
      <div class="field"><label>User name</label><input id="cr-user" autocomplete="off" placeholder="CORP\\deploy or deploy@corp.local">
        <div class="hint" id="cr-user-hint"></div></div>
      <div class="field"><label id="cr-pass-l">Password</label><input id="cr-pass" type="password" autocomplete="new-password"></div>
      <div class="field" id="cr-key-f"><label>Private key file (optional)</label><input id="cr-key" placeholder="C:\\Users\\me\\.ssh\\id_ed25519">
        <div class="hint">With a key, the password above is the key passphrase and sudo must not ask for a password (NOPASSWD) unless you log in as root.</div></div>
      <div class="field" id="cr-def-f"><label class="check"><input type="checkbox" id="cr-def"> Use for all <span id="cr-os">Windows</span> computers that don't have their own credential</label></div>`,
    [{ label: 'Cancel' }, {
      label: 'Save', cls: 'primary', onClick: async () => {
        const kind = $('#cr-kind').value;
        await POST('/credentials', {
          name: $('#cr-name').value.trim(), kind, username: $('#cr-user').value.trim(), secret: $('#cr-pass').value || null,
          key_path: kind === 'ssh' ? $('#cr-key').value.trim() || null : null,
          default_for: $('#cr-def').checked ? (kind === 'ssh' ? 'linux' : 'windows') : null,
        });
        toast('Credential saved');
        route();
      },
    }]);
    const vis = () => {
      const k = $('#cr-kind').value;
      $('#cr-key-f').style.display = k === 'ssh' ? '' : 'none';
      $('#cr-def-f').style.display = k === 'ldap' ? 'none' : '';
      $('#cr-os').textContent = k === 'ssh' ? 'Linux' : 'Windows';
      $('#cr-user').placeholder = k === 'ssh' ? 'deploy (or root)' : 'CORP\\deploy or deploy@corp.local';
      $('#cr-user-hint').textContent = k === 'windows' ? 'Leave user and password empty to use the account running PSCCM (Kerberos single sign-on).'
        : k === 'ldap' ? 'A normal domain user is enough. Use user@domain.' : 'Needs sudo rights unless it is root.';
    };
    $('#cr-kind').onchange = vis;
    vis();
  },
  delCred: act(async (id) => {
    if (!confirm('Delete this credential? Computers using it fall back to the default.')) return;
    await DEL('/credentials/' + id);
    route();
  }),
  saveAd: act(async () => { await PUT('/settings/ad', adForm()); toast('Saved'); }),
  testAd: act(async () => {
    $('#ad-result').textContent = 'Querying…';
    try {
      const r = await POST('/ad/preview', { settings: adForm() });
      const n = r.computers.length;
      $('#ad-result').innerHTML = `<span style="color:var(--ok)">✓ Found ${n} computer${n === 1 ? '' : 's'}</span>`;
    } catch (e) { $('#ad-result').innerHTML = `<span class="err">${esc(e.message)}</span>`; }
  }),
  saveWinrm: act(async () => { await PUT('/settings/winrm', { use_ssl: $('#w-ssl').checked, skip_ca_check: $('#w-skip').checked }); toast('Saved'); }),
  saveAgent: act(async () => { await PUT('/settings/agent', { enabled: $('#ag-enabled').checked, rotate: false }); toast('Saved'); }),
  agRotate: act(async () => {
    if (!confirm('Generate a new enrollment token? New installs must use it; existing agents are unaffected.')) return;
    await PUT('/settings/agent', { enabled: $('#ag-enabled') ? $('#ag-enabled').checked : true, rotate: true });
    route();
  }),
  agCopy() { navigator.clipboard.writeText($('#ag-cmd').textContent).then(() => toast('Copied')); },
  forgetHost: act(async () => {
    const h = $('#kh-host').value.trim();
    if (!h) return;
    await DEL('/known-hosts/' + encodeURIComponent(h));
    toast('Forgot host key for ' + h);
  }),
});

// --------------------------------------------------------------- inventory

const IV = { tab: 'computers', qs: { computers: '', software: '', outdated: '', changes: '' }, filter: '', sw: null, out: null, ch: null, chDays: 30, chKind: '' };
const INV_TABS = [['computers', 'Computers'], ['software', 'Software'], ['outdated', 'Outdated'], ['changes', 'Changes']];
const INV_SEARCH = {
  computers: 'Search name, OS, model, user, IP…', software: 'Search program or publisher…',
  outdated: 'Search program or computer…', changes: 'Search computer, program, version…',
};
const CHANGE_PILL = { installed: 's-success', removed: 's-failed', updated: 's-running', changed: 's-scheduled' };

function csvCell(v) {
  let s = String(v ?? '');
  // Excel and LibreOffice run a cell that starts with one of these as a formula.
  if (/^[=+\-@\t\r]/.test(s)) s = "'" + s;
  return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

/** Save rows as a CSV file (UTF-8 with BOM so Excel shows accents). */
function downloadCsv(name, header, rows) {
  const text = '﻿' + [header, ...rows].map((r) => r.map(csvCell).join(',')).join('\r\n');
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob([text], { type: 'text/csv;charset=utf-8' }));
  a.download = `psccm-${name}-${new Date().toISOString().slice(0, 10)}.csv`;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

/** A computer table with checkboxes (all ticked); read the choice with `pickedComputers`. */
function pickTable(rows, extraHead = '', extraCell = () => '') {
  return `<div class="table-wrap" style="max-height:45vh;overflow:auto"><table><thead><tr>
      <th style="width:30px"><input type="checkbox" checked onclick="$$('input[data-c]', this.closest('table')).forEach((i) => { i.checked = this.checked; })"></th>
      <th>Computer</th><th>Version</th>${extraHead}</tr></thead><tbody>
    ${rows.map((r) => `<tr><td><input type="checkbox" checked data-c="${esc(r.computer)}"></td><td><b>${esc(r.computer)}</b></td>
      <td class="small">${esc(r.version)}</td>${extraCell(r)}</tr>`).join('')}</tbody></table></div>`;
}
const pickedComputers = (root) => $$('input[data-c]:checked', root).map((i) => i.dataset.c);

/** Open New deployment with a package, variables and computers filled in, to review and start. */
async function deployPrefilled(pkgId, computers, vars = {}) {
  const { packages } = await loadPackages(true);
  if (!packages.some((p) => p.id === pkgId)) throw new Error(`The package '${pkgId}' is missing (deleted or disabled)`);
  if (!computers.length) throw new Error('Select at least one computer');
  Object.assign(W, { mode: 'package', pkg: pkgId, q: '', vars: { ...vars }, computers: new Set(computers), extra: '', when: '' });
  W.tags.clear();
  W.groups.clear();
  location.hash = '#/deploy';
}

const factLabel = (k) => (FACTS.find(([f]) => f === k) || [k, k])[1];

/** A change's old or new value as shown (memory in GB like elsewhere). */
const changeValue = (c, v) => (v && c.kind === 'changed' && c.name === 'ram_mb' ? `${(v / 1024).toFixed(1)} GB` : v);

function changeRow(c, withComputer) {
  const what = c.kind === 'changed' ? esc(factLabel(c.name)) : `<b>${esc(c.name)}</b>`;
  const [o, n] = [changeValue(c, c.old), changeValue(c, c.new)];
  return `<tr><td class="small" title="${esc(fmtTime(c.at))}">${ago(c.at)}</td>
    ${withComputer ? `<td><a href="javascript:void(0)" onclick="App.invShow(${jstr(c.computer)})">${esc(c.computer)}</a></td>` : ''}
    <td><span class="pill ${CHANGE_PILL[c.kind] || ''}">${esc(c.kind)}</span></td><td>${what}</td>
    <td class="small">${o ? esc(o) : ''}${o && n ? ' → ' : ''}${n ? esc(n) : ''}</td></tr>`;
}
const gbFmt = (b) => (b / 1073741824).toFixed(b < 10737418240 ? 1 : 0) + ' GB';
const lowDisk = (d) => d.size > 0 && d.free / d.size < 0.1;

/** The fixed disk with the least free space (by share). */
function worstDisk(inv) {
  return (inv.disks || []).filter((d) => d.size > 0).sort((a, b) => a.free / a.size - b.free / b.size)[0];
}

function diskBar(d) {
  const low = lowDisk(d);
  return `<div class="progress" title="${esc(d.mount)}: ${gbFmt(d.free)} free of ${gbFmt(d.size)}">
      <span class="${low ? 'p-bad' : 'p-ok'}" style="width:${(100 * (1 - d.free / d.size)).toFixed(0)}%"></span></div>
    <div class="small ${low ? 'err' : 'muted'}">${esc(d.mount)} ${gbFmt(d.free)} free of ${gbFmt(d.size)}</div>`;
}

function scanProgress(sc) {
  if (!sc.running) return '';
  const pct = sc.total ? (100 * sc.done) / sc.total : 0;
  return `<div class="card"><div class="card-body"><b>Scanning…</b> ${sc.done} of ${sc.total} computers done${sc.failed ? ` · <span class="err">${sc.failed} not reachable or failed</span>` : ''}
    <div class="progress" style="margin-top:8px"><span class="p-ok" style="width:${pct.toFixed(0)}%"></span></div></div></div>`;
}

function pollInventory(el) {
  poll(async () => {
    try {
      const r = await GET('/inventory');
      if (r.scan.running) {
        if ($('#inv-progress')) $('#inv-progress').innerHTML = scanProgress(r.scan);
        pollInventory(el);
      } else {
        IV.sw = null;
        IV.ch = null;
        toast(`Inventory scan finished${r.scan.failed ? ` (${r.scan.failed} not scanned)` : ''}`);
        pageInventory(el);
      }
    } catch { pollInventory(el); }
  }, 3000);
}

async function pageInventory(el) {
  const [r, computers, out] = await Promise.all([GET('/inventory'), GET('/computers'), GET('/inventory/outdated').catch(() => null)]);
  const inv = Object.fromEntries(r.inventory.map((i) => [i.computer.toLowerCase(), i]));
  const scanned = r.inventory.filter((i) => i.scanned_at);
  IV.out = out;
  const n = {
    reboot: scanned.filter((i) => i.facts.pending_reboot === 'true').length,
    disk: scanned.filter((i) => (i.disks || []).some(lowDisk)).length,
    failed: computers.filter((c) => { const i = inv[c.name.toLowerCase()]; return !i || i.status !== 'ok'; }).length,
    outdated: out ? new Set(out.apps.flatMap((a) => a.outdated.map((o) => o.computer.toLowerCase()))).size : 0,
  };
  const s = r.settings;
  const stat = (key, num, label, color) => `<div class="stat click ${IV.filter === key ? 'on' : ''}" onclick="App.invFilter('${key}')">
    <div class="n" style="color:${num && color ? color : 'inherit'}">${num}</div><div class="l">${label}</div></div>`;
  el.innerHTML = `
    <div class="page-head"><h1>Inventory</h1><span class="sub">${scanned.length} of ${computers.length} computers scanned</span>
      <div class="actions"><button class="primary" onclick="App.invScan()" ${r.scan.running || !computers.length ? 'disabled' : ''}>Scan all computers</button></div></div>
    <div id="inv-progress">${scanProgress(r.scan)}</div>
    <div class="stats">
      ${stat('', scanned.length, 'Scanned computers')}
      ${stat('reboot', n.reboot, 'Reboot pending', 'var(--warn)')}
      ${stat('disk', n.disk, 'A disk under 10% free', 'var(--bad)')}
      ${stat('failed', n.failed, 'Not scanned or last scan failed', 'var(--bad)')}
      <div class="stat click ${IV.tab === 'outdated' ? 'on' : ''}" onclick="App.invTab('outdated')">
        <div class="n" style="color:${n.outdated ? 'var(--warn)' : 'inherit'}">${n.outdated}</div><div class="l">Outdated App catalog software</div></div>
    </div>
    <div class="card"><div class="card-head">
      <div class="tabs">${INV_TABS.map(([k, l]) => `<button class="${IV.tab === k ? 'on' : ''}" onclick="App.invTab('${k}')">${l}</button>`).join('')}</div>
      <input class="search" id="inv-q" placeholder="${INV_SEARCH[IV.tab]}" value="${esc(IV.qs[IV.tab])}" oninput="App.invSearch(this.value)">
      <div class="right"><button onclick="App.invExport()" title="Download what this tab shows as a CSV file (opens in Excel)">Export CSV</button>
        ${IV.tab === 'software' ? '<button onclick="App.invExportAll()" title="One row per program per computer">Export every install</button>' : ''}</div>
    </div><div class="card-body flush" id="inv-body"></div></div>
    <div class="card"><div class="card-head"><h2>Scan settings</h2>
      ${s.last_scan ? `<span class="small muted">Last full scan started ${ago(s.last_scan)}</span>` : ''}</div><div class="card-body">
      <div class="row">
        <div class="field"><label>Scan all computers every (hours)</label><input id="iv-int" type="number" min="0" value="${s.interval_hours}">
          <div class="hint">0 = only when you click Scan. 24 is a good start.</div></div>
        <div class="field"><label>Computers scanned at the same time</label><input id="iv-conc" type="number" min="1" max="128" value="${s.concurrency}"></div>
      </div>
      <label class="check"><input type="checkbox" id="iv-re" ${s.rescan_after_deploy ? 'checked' : ''}> Re-scan computers after a deployment succeeds on them (only computers scanned before)</label>
      <p class="small muted">A scan runs a read-only script over WinRM/SSH with the computer's credential: OS, hardware, disks, logged-on user,
        pending reboot and installed programs. Nothing is installed or changed. Use the results in dynamic groups (e.g. "Chrome older than 130").</p>
      <button onclick="App.invSave()">Save</button></div></div>`;
  IV.data = { computers, inv };
  if (r.scan.running) pollInventory(el);
  await renderInventoryBody();
}

/** Computers tab rows after the stat filter and search. */
function inventoryRows() {
  const { computers, inv } = IV.data;
  const q = IV.qs.computers.toLowerCase();
  return computers.map((c) => ({ c, i: inv[c.name.toLowerCase()] })).filter(({ c, i }) => {
    const f = (i && i.facts) || {};
    if (IV.filter === 'reboot' && f.pending_reboot !== 'true') return false;
    if (IV.filter === 'disk' && !(i && i.scanned_at && (i.disks || []).some(lowDisk))) return false;
    if (IV.filter === 'failed' && i && i.status === 'ok') return false;
    return !q || [c.name, f.os_name, f.model, f.manufacturer, f.user, f.ip_addresses, f.cpu].some((v) => (v || '').toLowerCase().includes(q));
  });
}

const softwareRows = () => {
  const q = IV.qs.software.toLowerCase();
  return IV.sw.filter((s) => !q || s.name.toLowerCase().includes(q) || (s.publisher || '').toLowerCase().includes(q));
};

const outdatedRows = () => {
  const q = IV.qs.outdated.toLowerCase();
  return IV.out.apps.filter((a) => !q || a.name.toLowerCase().includes(q)
    || a.outdated.concat(a.current).some((o) => o.computer.toLowerCase().includes(q)));
};

const changeRows = () => {
  const q = IV.qs.changes.toLowerCase();
  return IV.ch.changes.filter((c) => (!IV.chKind || c.kind === IV.chKind)
    && (!q || [c.computer, c.name, factLabel(c.name), c.old, c.new].some((v) => (v || '').toLowerCase().includes(q))));
};

function renderInventoryBody() {
  if (IV.tab === 'software') return renderSoftware();
  if (IV.tab === 'outdated') return renderOutdated();
  if (IV.tab === 'changes') return renderChanges();
  const { computers } = IV.data;
  const rows = inventoryRows();
  $('#inv-body').innerHTML = !computers.length
    ? '<div class="empty"><b>No computers yet</b>Add computers first, then scan them.</div>'
    : `<div class="table-wrap"><table><thead><tr><th>Computer</th><th>Operating system</th><th>Hardware</th><th>Lowest disk</th>
        <th>Last boot</th><th>Logged on</th><th>Software</th><th>Scanned</th></tr></thead><tbody>
      ${rows.map(({ c, i }) => {
        const f = (i && i.facts) || {};
        const d = i && worstDisk(i);
        const ok = i && i.scanned_at;
        return `<tr class="click" onclick="App.invShow(${jstr(c.name)})">
          <td><b>${esc(c.name)}</b>${f.pending_reboot === 'true' ? ' <span class="pill s-partial">reboot pending</span>' : ''}
            ${f.ip_addresses ? `<div class="small muted">${esc(f.ip_addresses)}</div>` : ''}</td>
          <td>${ok ? `${esc(f.os_name || osLabel(c.os))}<div class="os">${esc([f.os_version, f.os_build || f.kernel].filter(Boolean).join(' · '))}</div>` : `<span class="muted">${osLabel(c.os)}</span>`}</td>
          <td class="small">${ok ? `${esc(f.model || '')}<div class="muted">${f.cpu_count ? esc(f.cpu_count) + ' CPU · ' : ''}${f.ram_mb ? (f.ram_mb / 1024).toFixed(0) + ' GB RAM' : ''}</div>` : ''}</td>
          <td style="min-width:150px">${d ? diskBar(d) : ''}</td>
          <td class="small" title="${esc(fmtTime(f.last_boot))}">${f.last_boot ? ago(f.last_boot) : ''}</td>
          <td class="small">${esc(f.user || '')}</td>
          <td class="num">${ok ? i.app_count : ''}</td>
          <td class="small">${!i ? '<span class="muted">never</span>'
            : `${ok ? ago(i.scanned_at) : '<span class="muted">never</span>'}${i.status !== 'ok' ? `<div class="err" title="${esc(i.error || '')}">${esc(i.status)} ${ago(i.last_attempt)}</div>` : ''}`}</td></tr>`;
      }).join('')}</tbody></table></div>
      ${rows.length ? '' : '<div class="empty">No computers match.</div>'}`;
}

async function renderSoftware() {
  if (!IV.sw) {
    $('#inv-body').innerHTML = '<div class="empty">Loading…</div>';
    IV.sw = (await GET('/inventory/software')).software;
  }
  const list = softwareRows();
  const shown = list.slice(0, 500);
  $('#inv-body').innerHTML = !IV.sw.length
    ? '<div class="empty"><b>No software inventory yet</b>Scan computers to see what is installed.</div>'
    : `<div class="table-wrap"><table><thead><tr><th>Program</th><th>Publisher</th><th>Versions</th><th>Computers</th></tr></thead><tbody>
      ${shown.map((s, i) => `<tr class="click" onclick="App.invSoftware(${IV.sw.indexOf(list[i])})">
        <td><b>${esc(s.name)}</b></td><td class="small muted">${esc(s.publisher)}</td>
        <td>${s.versions.slice(0, 6).map((v) => `<span class="tag" title="${v.computers} computer(s)">${esc(v.version || '?')} · ${v.computers}</span>`).join('')}${s.versions.length > 6 ? `<span class="small muted"> +${s.versions.length - 6} more</span>` : ''}</td>
        <td class="num">${s.computers}</td></tr>`).join('')}</tbody></table></div>
      ${list.length > shown.length ? `<div class="empty">Showing ${shown.length} of ${list.length}; refine the search.</div>` : ''}
      ${list.length ? '' : '<div class="empty">No programs match.</div>'}`;
}

function renderOutdated() {
  const r = IV.out;
  if (!r) {
    $('#inv-body').innerHTML = '<div class="empty">Could not compare with the App catalog.</div>';
    return;
  }
  const list = outdatedRows();
  const note = r.last_check ? '' : `<div class="card-body small">The latest versions have not been looked up yet.
    <button onclick="App.invCatalogCheck()">Check for updates</button></div>`;
  const action = (a, idx) => {
    if (!a.outdated.length) return '';
    if (!a.added) return `<button onclick="event.stopPropagation(); App.invCatalogAdd(${jstr(a.id)})" title="Download the installer and create its package">Add to catalog</button>`;
    if (!a.package_version) return '<span class="small muted">downloading…</span>';
    return `<button class="primary" onclick="event.stopPropagation(); App.invUpdate(${idx})">Update ${a.outdated.length}</button>`;
  };
  $('#inv-body').innerHTML = note + (!r.apps.length
    ? `<div class="empty"><b>No App catalog programs found</b>None of the ${r.scanned} scanned Windows computers has a program from the App catalog.</div>`
    : `<div class="table-wrap"><table><thead><tr><th>Program</th><th>Latest</th><th>Package installs</th><th>Up to date</th><th>Outdated</th><th></th></tr></thead><tbody>
      ${list.map((a) => {
        const idx = r.apps.indexOf(a);
        const versions = [...new Set(a.outdated.map((o) => o.version || '?'))];
        return `<tr class="click" onclick="App.invOutdated(${idx})">
          <td><b>${esc(a.name)}</b>${versions.length ? `<div class="small muted">has ${versions.slice(0, 4).map(esc).join(', ')}${versions.length > 4 ? ` +${versions.length - 4} more` : ''}</div>` : ''}</td>
          <td>${a.latest ? esc(a.latest) : '<span class="muted">unknown</span>'}</td>
          <td class="small">${!a.added ? '<span class="muted">not added</span>' : a.package_version
            ? `${esc(a.package_version)}${a.package_behind ? ' <span class="pill s-partial" title="Update the app on the App catalog page to install the latest version">older than latest</span>' : ''}` : '<span class="muted">downloading…</span>'}</td>
          <td class="num">${a.current.length}</td>
          <td class="num">${a.outdated.length ? `<b class="err">${a.outdated.length}</b>` : '0'}</td>
          <td style="text-align:right">${action(a, idx)}</td></tr>`;
      }).join('')}</tbody></table></div>
      ${list.length ? '' : '<div class="empty">Nothing matches.</div>'}
      <div class="card-body small muted">Compares the programs found by each App catalog entry's name pattern on scanned Windows computers with the latest version.
        A computer counts as up to date when any matching program has that version or newer, the same rule the package uses to skip computers.</div>`);
}

async function renderChanges() {
  if (!IV.ch) {
    $('#inv-body').innerHTML = '<div class="empty">Loading…</div>';
    IV.ch = await GET(`/inventory/changes?days=${IV.chDays}`);
  }
  const list = changeRows();
  const shown = list.slice(0, 1000);
  const kinds = [['', 'All'], ['installed', 'Installed'], ['removed', 'Removed'], ['updated', 'Updated'], ['changed', 'OS / hardware']];
  $('#inv-body').innerHTML = `<div class="card-body" style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
      <select style="width:auto" onchange="App.invChDays(+this.value)">
        ${[7, 30, 90, 365].map((d) => `<option value="${d}" ${IV.chDays === d ? 'selected' : ''}>Last ${d} days</option>`).join('')}</select>
      <div class="chips">${kinds.map(([k, l]) => `<span class="chip ${IV.chKind === k ? 'on' : ''}" onclick="App.invChKind('${k}')">${l}</span>`).join('')}</div>
      <span class="small muted">${list.length} change${list.length === 1 ? '' : 's'}${IV.ch.truncated ? ' (only the newest are loaded)' : ''}</span></div>
    ${!IV.ch.changes.length
      ? `<div class="empty"><b>No changes in the last ${IV.chDays} days</b>Changes are recorded from a computer's second scan on: software installed, removed or updated, and OS, memory or hardware changes.</div>`
      : `<div class="table-wrap"><table><thead><tr><th>When</th><th>Computer</th><th>Change</th><th>Program or item</th><th>Version / value</th></tr></thead><tbody>
        ${shown.map((c) => changeRow(c, true)).join('')}</tbody></table></div>
        ${list.length > shown.length ? `<div class="empty">Showing ${shown.length} of ${list.length}; refine the search or export the rest.</div>` : ''}
        ${list.length ? '' : '<div class="empty">No changes match.</div>'}`}`;
}

/** Which uninstall package fits the computers, and its variables. */
function uninstallPlan(name, computers) {
  const os = new Set(computers.map((n) => (IV.data.computers.find((c) => c.name.toLowerCase() === n.toLowerCase()) || {}).os));
  os.delete('unknown');
  os.delete(undefined);
  if (os.size > 1) throw new Error('Select only Windows or only Linux computers: they are uninstalled with different packages');
  if (os.has('linux')) {
    if (!/^[A-Za-z0-9][A-Za-z0-9.+_:-]*$/.test(name)) throw new Error(`'${name}' is not a package name the package manager can remove`);
    return { pkg: 'linux-package', vars: { packages: name, action: 'remove' } };
  }
  // ' ends the PowerShell string; [ ] and ` are -like wildcards: match any character there instead.
  return { pkg: 'uninstall-app', vars: { name: name.replace(/['[\]`]/g, '?') } };
}

const FACTS = [
  ['os_name', 'Operating system'], ['os_version', 'Version'], ['os_build', 'Build'], ['kernel', 'Kernel'], ['arch', 'Architecture'],
  ['manufacturer', 'Manufacturer'], ['model', 'Model'], ['serial', 'Serial number'], ['cpu', 'Processor'], ['cpu_count', 'Logical CPUs'],
  ['ram_mb', 'Memory'], ['domain', 'Domain'], ['user', 'Logged-on user'], ['ip_addresses', 'IPv4 addresses'], ['last_boot', 'Last boot'],
  ['install_date', 'OS installed'], ['last_patch', 'Last update installed'], ['last_patch_id', 'Last update'],
  ['pending_reboot', 'Reboot pending'], ['package_manager', 'Package manager'],
];

function factValue(k, v) {
  if (k === 'ram_mb') return `${(v / 1024).toFixed(1)} GB`;
  if (k === 'last_boot' || k === 'install_date') return `${esc(fmtTime(v))} <span class="muted">(${ago(v)})</span>`;
  if (k === 'pending_reboot') return v === 'true' ? '<span class="pill s-partial">yes</span>' : 'no';
  return esc(v);
}

Object.assign(App, {
  invTab(t) { IV.tab = t; route(); },
  invFilter(f) { IV.filter = IV.filter === f ? '' : f; IV.tab = 'computers'; route(); },
  invSearch(v) { IV.qs[IV.tab] = v; renderInventoryBody(); },
  invChDays(d) { IV.chDays = d; IV.ch = null; renderInventoryBody(); },
  invChKind(k) { IV.chKind = k; renderInventoryBody(); },
  invExport: act(async () => {
    if (IV.tab === 'computers') {
      const keys = ['os_name', 'os_version', 'os_build', 'kernel', 'arch', 'manufacturer', 'model', 'serial', 'cpu', 'cpu_count', 'ram_mb',
        'domain', 'user', 'ip_addresses', 'last_boot', 'pending_reboot', 'last_patch', 'last_patch_id'];
      downloadCsv('computers', ['Computer', 'OS', ...keys.map(factLabel), 'Disks', 'Programs', 'Scanned', 'Last attempt', 'Status', 'Error'],
        inventoryRows().map(({ c, i }) => {
          const f = (i && i.facts) || {};
          const disks = ((i && i.disks) || []).map((d) => `${d.mount} ${gbFmt(d.free)} free of ${gbFmt(d.size)}`).join('; ');
          return [c.name, osLabel(c.os), ...keys.map((k) => f[k]), disks, i && i.scanned_at ? i.app_count : '',
            i ? fmtTime(i.scanned_at) : '', i ? fmtTime(i.last_attempt) : '', i ? i.status : 'never scanned', (i && i.error) || ''];
        }));
    } else if (IV.tab === 'software') {
      if (!IV.sw) IV.sw = (await GET('/inventory/software')).software;
      downloadCsv('software', ['Program', 'Publisher', 'Version', 'Computers'],
        softwareRows().flatMap((s) => s.versions.map((v) => [s.name, s.publisher, v.version, v.computers])));
    } else if (IV.tab === 'outdated') {
      if (!IV.out) throw new Error('Nothing to export');
      downloadCsv('outdated', ['Program', 'Computer', 'Installed version', 'Latest version', 'Up to date'],
        outdatedRows().flatMap((a) => [...a.outdated.map((o) => [a.name, o.computer, o.version, a.latest, 'no']),
          ...a.current.map((o) => [a.name, o.computer, o.version, a.latest, a.latest ? 'yes' : ''])]));
    } else {
      if (!IV.ch) IV.ch = await GET(`/inventory/changes?days=${IV.chDays}`);
      downloadCsv('changes', ['Time', 'Computer', 'Change', 'Program or item', 'Old', 'New'],
        changeRows().map((c) => [fmtTime(c.at), c.computer, c.kind, c.kind === 'changed' ? factLabel(c.name) : c.name,
          changeValue(c, c.old), changeValue(c, c.new)]));
    }
  }),
  invExportAll: act(async () => {
    const r = await GET('/inventory/software/all');
    downloadCsv('installed-software', ['Computer', 'Program', 'Version', 'Publisher'], r.software.map((a) => [a.computer, a.name, a.version, a.publisher]));
  }),
  invOutdated(idx) {
    const a = IV.out.apps[idx];
    const canDeploy = a.added && a.package_version;
    modal(a.name, `
      <div class="small muted" style="margin-bottom:10px">Latest version ${a.latest ? `<b>${esc(a.latest)}</b>` : 'unknown'}${a.checked_at ? ` (looked up ${ago(a.checked_at)})` : ''}.
        ${!a.added ? 'This app is not added to the App catalog yet, so there is no package to deploy.'
          : a.package_version ? `The package <code>${esc(a.package_id)}</code> installs ${esc(a.package_version)}${a.package_behind ? ': update the app on the App catalog page first to install the latest version' : ''}.`
            : 'The installer is still downloading.'}</div>
      ${a.outdated.length ? `<div class="cat">Outdated (${a.outdated.length})</div>${pickTable(a.outdated)}` : ''}
      ${a.current.length ? `<div class="cat">${a.latest ? 'Up to date' : 'Installed'} (${a.current.length})</div>
        <div class="table-wrap" style="max-height:25vh;overflow:auto"><table><tbody>
        ${a.current.map((o) => `<tr><td><b>${esc(o.computer)}</b></td><td class="small">${esc(o.version)}</td></tr>`).join('')}</tbody></table></div>` : ''}`,
    [
      { label: 'Close' },
      ...(a.outdated.length && !a.added ? [{ label: 'Add to catalog', onClick: () => App.invCatalogAdd(a.id) }] : []),
      ...(a.outdated.length && canDeploy ? [{
        label: 'Update selected…', cls: 'primary',
        onClick: async (root) => { await deployPrefilled(a.package_id, pickedComputers(root)); },
      }] : []),
    ], { wide: true });
  },
  invUpdate: act(async (idx) => {
    const a = IV.out.apps[idx];
    await deployPrefilled(a.package_id, a.outdated.map((o) => o.computer));
  }),
  invCatalogAdd: act(async (id) => {
    await POST(`/catalog/${encodeURIComponent(id)}/add`);
    toast('Downloading the installer. Update the computers from Inventory → Outdated once it is ready.');
    location.hash = '#/catalog';
  }),
  invCatalogCheck: act(async () => {
    await POST('/catalog/check');
    toast('Looking up the latest versions…');
    location.hash = '#/catalog';
  }),
  invScan: act(async (names) => {
    const r = await POST('/inventory/scan', { names: names || null });
    toast(`Scanning ${r.started} computer(s)…`);
    route();
  }),
  invSave: act(async () => {
    await PUT('/settings/inventory', {
      interval_hours: +$('#iv-int').value || 0, concurrency: +$('#iv-conc').value || 16, rescan_after_deploy: $('#iv-re').checked,
    });
    toast('Saved');
  }),
  invShow: act(async (name) => {
    const [i, ch] = await Promise.all([
      GET('/inventory/computer/' + encodeURIComponent(name)).catch(() => null),
      GET(`/inventory/changes?days=365&limit=300&computer=${encodeURIComponent(name)}`).catch(() => ({ changes: [] })),
    ]);
    const scanBtn = { label: 'Scan now', cls: 'primary', onClick: async () => { await App.invScan([name]); } };
    if (!i) {
      modal(name, '<p class="muted">This computer has not been scanned yet.</p>', [{ label: 'Close' }, scanBtn]);
      return;
    }
    const f = i.facts;
    const known = new Set(FACTS.map(([k]) => k));
    const rows = FACTS.filter(([k]) => f[k]).concat(Object.keys(f).filter((k) => !known.has(k)).map((k) => [k, k]));
    const m = modal(name, `
      <div class="small muted" style="margin-bottom:10px">${i.scanned_at ? `Scanned ${fmtTime(i.scanned_at)} (${ago(i.scanned_at)})` : 'Never scanned successfully'}
        ${i.status !== 'ok' ? `<div class="err">Last attempt ${ago(i.last_attempt)}: ${esc(i.status)} — ${esc(i.error || '')}</div>` : ''}</div>
      <div class="grid two" style="gap:18px">
        <dl class="facts">${rows.map(([k, l]) => `<dt>${esc(l)}</dt><dd>${factValue(k, f[k])}</dd>`).join('')}</dl>
        <div>${(i.disks || []).map((d) => `<div style="margin-bottom:10px">${diskBar(d)}</div>`).join('') || '<span class="muted small">No disks reported</span>'}</div>
      </div>
      <div class="cat">Installed software (${i.apps.length})</div>
      <input class="search" id="inv-apps-q" placeholder="Filter…" style="margin-bottom:8px">
      <div class="table-wrap" style="max-height:40vh;overflow:auto"><table><thead><tr><th>Program</th><th>Version</th><th>Publisher</th></tr></thead><tbody>
        ${i.apps.map((a) => `<tr data-q="${esc((a.name + ' ' + a.publisher).toLowerCase())}"><td>${esc(a.name)}</td><td class="small">${esc(a.version)}</td>
          <td class="small muted">${esc(a.publisher)}</td></tr>`).join('')}</tbody></table></div>
      <div class="cat">Changes in the last year (${ch.changes.length}${ch.truncated ? '+' : ''})</div>
      ${ch.changes.length ? `<div class="table-wrap" style="max-height:30vh;overflow:auto"><table><tbody>
        ${ch.changes.map((c) => changeRow(c, false)).join('')}</tbody></table></div>`
        : '<div class="small muted">No changes recorded. Changes are recorded from the second scan on.</div>'}`,
    [{ label: 'Close' }, scanBtn], { wide: true });
    $('#inv-apps-q', m.root).oninput = (e) => {
      const q = e.target.value.toLowerCase();
      $$('tbody tr', m.root).forEach((tr) => { tr.style.display = tr.dataset.q.includes(q) ? '' : 'none'; });
    };
  }),
  invSoftware: act(async (idx) => {
    const s = IV.sw[idx];
    const r = await GET('/inventory/software/computers?name=' + encodeURIComponent(s.name));
    const group = (missing) => ({
      label: missing ? 'Group: computers without it' : 'Group: computers with it',
      onClick: () => {
        setTimeout(() => App.editGroup(0, {
          name: `${missing ? 'Without' : 'With'} ${s.name}`.slice(0, 60), kind: 'dynamic',
          description: 'Created from the software inventory', rules: { tags: [], app: s.name, app_missing: missing },
        }), 0);
      },
    });
    const uninstall = {
      label: 'Uninstall from selected…', cls: 'danger',
      onClick: async (root) => {
        const picked = pickedComputers(root);
        if (!picked.length) throw new Error('Select at least one computer');
        const plan = uninstallPlan(s.name, picked);
        await deployPrefilled(plan.pkg, picked, plan.vars);
      },
    };
    const exportCsv = {
      label: 'Export CSV',
      onClick: () => { downloadCsv('software-computers', ['Program', 'Computer', 'Version'], r.computers.map((c) => [s.name, c.computer, c.version])); return false; },
    };
    modal(s.name, `
      <div class="small muted" style="margin-bottom:10px">${esc(s.publisher || '')} · installed on ${r.computers.length} computer(s).
        Groups made here are dynamic: add a version condition (e.g. <code>&lt;130</code>) to find outdated copies.
        <i>Uninstall</i> opens a deployment to review before anything runs.</div>
      ${pickTable(r.computers)}`,
    [group(true), group(false), exportCsv, uninstall, { label: 'Close' }], { wide: true });
  }),
});

// ------------------------------------------------------------- group policy

let GPO_AD = null; // GPO list from AD (loaded on demand)

async function pageGpo(el) {
  const { gpos: imported } = await GET('/gpos/imported');
  const byGuid = Object.fromEntries(imported.map((m) => [m.guid, m]));
  const rows = GPO_AD || imported.map((m) => ({ guid: m.guid, name: m.name, computer_version: null, links: null, imported: m }));
  el.innerHTML = `<div class="page-head"><h1>Group Policy</h1>
      <span class="sub">Import GPOs read-only from AD and apply their computer settings directly to computers — no GPO link or AD change needed</span>
      <div class="actions"><button onclick="App.gpoFolder()">Import GPO backup folder</button>
        <button class="primary" onclick="App.gpoLoad()">${GPO_AD ? 'Refresh from AD' : 'Load GPOs from AD'}</button></div></div>
    <div class="card"><div class="card-body small muted">
      Applied: Administrative Templates / registry policies, security settings (secedit) and advanced audit policy.
      Not applied: Group Policy Preferences, scripts, software installation and user settings (they are listed per GPO).
      Each imported GPO becomes a package <code>gpo-…</code> you can deploy, schedule (to keep computers compliant) or run in <i>remove</i> mode.
    </div></div>
    <div class="card"><div class="card-body flush">${rows.length ? `<div class="table-wrap"><table><thead><tr>
        <th>GPO</th><th>AD version</th><th>Linked to</th><th>Imported</th><th></th></tr></thead><tbody>
      ${rows.map((g) => {
        const m = g.imported || byGuid[g.guid];
        const outdated = m && g.computer_version !== null && g.computer_version !== m.computer_version;
        return `<tr><td><b>${esc(g.name)}</b><div class="small muted"><code>{${esc(g.guid)}}</code></div>
            ${g.computer_enabled === false ? '<span class="tag">computer settings disabled</span>' : ''}</td>
          <td class="small">${g.computer_version ?? '<span class="muted">—</span>'}${g.changed ? `<div class="muted">${ago(g.changed)}</div>` : ''}</td>
          <td class="small">${g.links === null ? '<span class="muted">load from AD</span>' : g.links.length ? g.links.map((l) =>
            `<div title="${esc(l.target)}">${esc(l.target.split(',')[0].replace(/^(OU|DC)=/i, ''))}${l.enforced ? ' <span class="tag">enforced</span>' : ''}${l.enabled ? '' : ' <span class="tag">link off</span>'}</div>`).join('') : '<span class="muted">not linked</span>'}</td>
          <td class="small">${m ? `${pill(outdated ? 'partial' : 'success').replace(/>(partial|success)</, `>${outdated ? 'outdated' : 'v' + m.computer_version}<`)}
            <div class="muted">${m.machine_settings} registry · ${m.has_security ? 'security · ' : ''}${m.has_audit ? 'audit · ' : ''}${ago(m.imported_at)}</div>` : '<span class="muted">no</span>'}</td>
          <td style="white-space:nowrap">
            ${g.computer_version !== null ? `<button onclick="App.gpoImport(${jstr(g.guid)})">${m ? 'Re-import' : 'Import'}</button>` : ''}
            ${m ? `<button onclick="App.gpoView(${jstr(g.guid)})">View</button>
              <button class="primary" onclick="App.deployPackage(${jstr(m.package_id)})">Deploy</button>` : ''}
          </td></tr>`;
      }).join('')}</tbody></table></div>`
      : '<div class="empty"><b>No GPOs imported yet</b>Click "Load GPOs from AD" (uses the Active Directory settings).</div>'}</div></div>`;
}

Object.assign(App, {
  gpoLoad: act(async () => {
    toast('Reading GPOs from Active Directory…');
    GPO_AD = (await GET('/gpos')).gpos;
    route();
  }),
  gpoFolder() {
    modal('Import a GPO backup folder', `<p class="small muted">A folder made by GPMC "Back Up…" or <code>Backup-GPO</code> (the {GUID} folder), or a copy of a GPO folder from SYSVOL. Useful when the console cannot reach AD.</p>
      <div class="field"><label>Folder on the console computer</label><input id="gf-path" placeholder="C:\GPOBackups\{31B2F340-...}"></div>
      <div class="field"><label>Name (optional)</label><input id="gf-name" placeholder="read from the backup"></div>`,
    [{ label: 'Cancel' }, { label: 'Import', cls: 'primary', onClick: async () => {
      const m = await POST('/gpos/import-folder', { path: $('#gf-path').value, name: $('#gf-name').value || null });
      PKGS = null;
      toast(`Imported "${m.name}" as package ${m.package_id}`);
      route();
    } }]);
  },
  gpoImport: act(async (guid) => {
    toast('Copying GPO from SYSVOL…');
    const m = await POST(`/gpos/${guid}/import`);
    PKGS = null;
    toast(`Imported "${m.name}" as package ${m.package_id}`);
    if (GPO_AD) GPO_AD = GPO_AD.map((g) => (g.guid === m.guid ? { ...g, imported: m } : g));
    route();
  }),
  gpoView: act(async (guid) => {
    const { meta, snapshot: s } = await GET('/gpos/' + guid);
    const val = (v) => (Array.isArray(v) ? v.join(' | ') : v === null ? '' : String(v));
    modal('GPO: ' + meta.name, `
      <div class="small muted" style="margin-bottom:10px">Imported ${fmtTime(meta.imported_at)} from <code>${esc(meta.source_path)}</code> · version ${meta.computer_version} · package <a href="#/package/${esc(meta.package_id)}">${esc(meta.package_id)}</a></div>
      <div class="cat">Registry policies (${s.machine.length})</div>
      ${s.machine.length ? `<div class="table-wrap" style="max-height:40vh;overflow:auto"><table><thead><tr><th>Action</th><th>Key</th><th>Value</th><th>Data</th></tr></thead><tbody>
        ${s.machine.map((e) => `<tr><td class="small">${esc(e.a)}</td><td class="small"><code>HKLM\\${esc(e.k)}</code></td><td class="small">${esc(e.n)}</td>
          <td class="small"><code>${esc(val(e.v))}</code> <span class="muted">${esc(e.t)}</span></td></tr>`).join('')}</tbody></table></div>` : '<div class="muted small">None</div>'}
      ${s.security ? `<div class="cat">Security settings (GptTmpl.inf)</div><pre class="log" style="min-height:0;max-height:30vh;border-radius:8px">${esc(s.security)}</pre>` : ''}
      ${s.audit ? `<div class="cat">Audit policy (audit.csv)</div><pre class="log" style="min-height:0;max-height:20vh;border-radius:8px">${esc(s.audit)}</pre>` : ''}
      ${s.user_count ? `<div class="cat">User settings</div><div class="small">${s.user_count} user registry settings — not applied (user policy needs the GPO linked in AD).</div>` : ''}
      ${s.unsupported.length ? `<div class="cat">Not applied by PSCCM</div><ul class="small">${s.unsupported.map((u) => `<li><code>${esc(u)}</code></li>`).join('')}</ul>` : ''}`,
    [{ label: 'Delete import', cls: 'danger', onClick: async () => {
      if (!confirm('Delete this imported snapshot and its package? (Nothing changes in AD.)')) return false;
      await DEL('/gpos/' + guid);
      PKGS = null;
      route();
    } }, { label: 'Close' }], { wide: true });
  }),
});

// ------------------------------------------------------------- app catalog

const catPill = (cls, text) => `<span class="pill s-${esc(cls)}">${esc(text)}</span>`;

function catalogStatus(a) {
  const cur = a.current;
  if (a.busy) {
    const { done, total } = a.busy;
    return catPill('running', total ? `downloading ${Math.round((100 * done) / total)}%` : done ? `downloading ${sizeFmt(done)}` : 'looking up…');
  }
  if (!a.added) return '<span class="muted">not added</span>';
  if (!cur) return catPill('failed', 'not downloaded');
  return `${a.update_available ? catPill('scheduled', `${cur.version} → ${a.latest}`) : catPill('success', cur.version)}
    <div class="small muted">${sizeFmt(cur.size)} · ${cur.verified ? 'SHA-256 matches the vendor' : 'HTTPS download'} · ${ago(cur.downloaded_at)}</div>`;
}

async function pageCatalog(el) {
  const c = await GET('/catalog');
  PKGS = null; // catalog packages come and go
  if (!c.last_check && !c.checking) {
    await POST('/catalog/check');
    c.checking = true;
  }
  const row = (a) => `<tr>
      <td style="min-width:240px"><b>${esc(a.name)}</b>${a.homepage ? ` <a class="small" href="${esc(a.homepage)}" target="_blank" rel="noopener">site</a>` : ''}
        <div class="small muted">${esc(a.description)}</div>
        ${a.error ? `<div class="small err">${esc(a.error)}</div>` : ''}</td>
      <td class="small">${a.latest ? `<b>${esc(a.latest)}</b>` : '<span class="muted">—</span>'}${a.checked_at ? `<div class="muted">${ago(a.checked_at)}</div>` : ''}</td>
      <td class="small">${catalogStatus(a)}</td>
      <td class="small">${a.added ? (a.deploy_groups.length ? a.deploy_groups.map((g) => `<span class="tag">${esc(g)}</span>`).join(' ') : '<span class="muted">manual</span>') : ''}</td>
      <td><div class="row" style="gap:6px;justify-content:flex-end">${!a.added
        ? `<button class="primary" onclick="App.catAdd(${jstr(a.id)})" ${a.busy ? 'disabled' : ''}>Add</button>`
        : `${a.update_available || !a.current || a.error ? `<button onclick="App.catUpdate(${jstr(a.id)})" ${a.busy ? 'disabled' : ''}>${a.current ? 'Update' : 'Retry'}</button>` : ''}
          <button class="primary" onclick="App.deployPackage(${jstr(a.package_id)})" ${a.current && !a.busy ? '' : 'disabled'}>Deploy</button>
          <button onclick="App.catGroups(${jstr(a.id)})">Auto-deploy</button>
          <button class="danger" onclick="App.catRemove(${jstr(a.id)})" ${a.busy ? 'disabled' : ''}>Remove</button>`}</div></td></tr>`;
  el.innerHTML = `<div class="page-head"><h1>App catalog</h1>
      <span class="sub">Common apps at their latest version — downloaded once to the repository, checked and kept as ready-to-deploy packages</span>
      <div class="actions"><button class="primary" onclick="App.catCheck()" ${c.checking ? 'disabled' : ''}>${c.checking ? 'Checking…' : 'Check for updates'}</button></div></div>
    ${c.errors.length ? `<div class="card"><div class="card-body err">${c.errors.map(esc).join('<br>')}</div></div>` : ''}
    <div class="card"><div class="card-body small">
      <label class="check"><input type="checkbox" ${c.auto_check ? 'checked' : ''} onchange="App.catAuto(this.checked)">
        Check daily and download new versions of added apps (and deploy them to their auto-deploy groups)</label>
      <div class="muted" style="margin-top:6px">Last check: ${c.checking ? 'running now' : c.last_check ? ago(c.last_check) : 'never'}.
        <b>Add</b> downloads the 64-bit installer and creates a package <code>catalog-…</code> in the "App catalog" category.
        Computers get the installer pushed from the console, so they need no internet access, and computers that already have the version are skipped.
        Add your own apps in <code>${esc(c.custom_file)}</code> (same format as the built-in catalog).</div>
    </div></div>
    <div class="card"><div class="card-body flush"><div class="table-wrap"><table><thead><tr>
        <th>App</th><th>Latest</th><th>In PSCCM</th><th>Auto-deploy to</th><th></th></tr></thead><tbody>
      ${groupByCategory(c.apps).map(([cat, apps]) => `<tr><td colspan="5"><div class="cat" style="margin:4px 0 0">${esc(cat || 'Other')}</div></td></tr>${apps.map(row).join('')}`).join('')}
    </tbody></table></div></div></div>`;
  if (c.checking || c.apps.some((a) => a.busy)) poll(() => pageCatalog(el).catch(() => {}), 1500);
}

Object.assign(App, {
  catCheck: act(async () => {
    await POST('/catalog/check');
    toast('Looking up the latest versions…');
    pageCatalog($('#main'));
  }),
  catAuto: act(async (on) => {
    await PUT('/catalog/settings', { auto_check: on });
    toast(on ? 'Daily check on' : 'Daily check off');
  }),
  catAdd: act(async (id) => {
    await POST(`/catalog/${id}/add`);
    toast('Downloading the latest version…');
    pageCatalog($('#main'));
  }),
  catUpdate: act(async (id) => {
    await POST(`/catalog/${id}/update`);
    pageCatalog($('#main'));
  }),
  catRemove: act(async (id) => {
    if (!confirm('Remove this app\'s package and downloaded installers? (Nothing is uninstalled from computers.)')) return;
    await DEL('/catalog/' + id);
    pageCatalog($('#main'));
  }),
  catGroups: act(async (id) => {
    const [{ apps }, { groups }] = await Promise.all([GET('/catalog'), GET('/groups')]);
    const a = apps.find((x) => x.id === id);
    const on = new Set(a.deploy_groups);
    modal('Auto-deploy ' + a.name, `
      <p class="small">When a new version is downloaded, deploy it to these groups right away.
        Computers that already have that version or newer are skipped, so this also installs it on new computers in the group.</p>
      <div class="field"><label>Groups</label><div class="chips">
        ${groups.map((g) => `<span class="chip ${on.has(g.name) ? 'on' : ''}" data-group="${esc(g.name)}" onclick="this.classList.toggle('on')">${g.kind === 'dynamic' ? '⚡ ' : ''}${esc(g.name)} <span class="small">${g.members.length}</span></span>`).join('')
          || '<span class="muted small">No groups yet — create one on the <a href="#/computers">Computers</a> page.</span>'}
      </div></div>
      <p class="small muted">To roll out the current version now, use <b>Deploy</b>. For a fixed day and time instead, leave this empty and add a weekly schedule for <code>${esc(a.package_id)}</code>.</p>`,
    [{ label: 'Cancel' }, { label: 'Save', cls: 'primary', onClick: async (root) => {
      const deploy_groups = $$('.chip.on', root).map((x) => x.dataset.group);
      await PUT('/catalog/' + id, { deploy_groups });
      toast(deploy_groups.length ? `New versions go to ${deploy_groups.join(', ')}` : 'Auto-deploy off');
      pageCatalog($('#main'));
    } }]);
  }),
});

// ----------------------------------------------------------------- updates

async function loadUpdateBadge() {
  try {
    const u = await GET('/update');
    const latest = u.settings.latest;
    $('#side-foot').innerHTML = `PSCCM v${esc(u.current)}${latest && latest.newer ? `<br><a href="#/settings" style="color:#fbbf24">Update ${esc(latest.version)} available</a>` : ''}`;
  } catch { /* not critical */ }
}

Object.assign(App, {
  updCheck: act(async () => {
    const r = await POST('/update/check');
    toast(r.newer ? `Version ${r.version} is available` : `You have the latest version (${r.version})`);
    loadUpdateBadge();
    route();
  }),
  updInstall: act(async () => {
    if (!confirm('Download, verify and install the new version now? The console restarts.')) return;
    const r = await POST('/update/install');
    toast(r.message);
  }),
  updSave: act(async () => {
    const u = await GET('/update');
    await PUT('/settings/update', { ...u.settings, auto_check: $('#u-check').checked, auto_install: $('#u-install').checked, repo: $('#u-repo').value.trim() || u.settings.repo });
    toast('Saved');
  }),
});

function updateCard(u) {
  const l = u.settings.latest;
  return `<div class="card"><div class="card-head"><h2>Updates</h2><span class="small muted">Installed: v${esc(u.current)}</span>
      <div class="right"><button onclick="App.updCheck()">Check now</button>
        ${l && l.newer && !l.signature_error ? `<button class="primary" onclick="App.updInstall()">Install ${esc(l.version)}</button>` : ''}</div></div>
    <div class="card-body">
      ${l ? `<p style="margin-top:0">Latest release: <a href="${esc(l.url)}" target="_blank" rel="noopener">v${esc(l.version)}</a>
        ${l.newer ? pill('scheduled').replace('scheduled', 'update available') : pill('success').replace('success', 'up to date')}
        <span class="small muted">checked ${ago(u.settings.last_check)}</span></p>` : '<p class="muted" style="margin-top:0">Not checked yet.</p>'}
      ${l && l.newer && l.signature_error ? `<p class="small" style="color:var(--bad)">This release will not be installed: ${esc(l.signature_error)}.</p>` : ''}
      ${u.platform === 'windows' && !u.installed ? '<p class="small muted">Running the portable exe: installing an update runs the setup program, which installs PSCCM as a service in Program Files.</p>' : ''}
      <label class="check"><input type="checkbox" id="u-check" ${u.settings.auto_check ? 'checked' : ''}> Check for updates daily</label>
      <label class="check"><input type="checkbox" id="u-install" ${u.settings.auto_install ? 'checked' : ''}> Install updates automatically (when no deployment is running)</label>
      <div class="row" style="margin-top:8px"><div class="field"><label>Release source (GitHub owner/repo)</label><input id="u-repo" value="${esc(u.settings.repo)}"></div></div>
      <p class="small muted">A release is only installed when its SHA256SUMS carries a valid signature by the PSCCM release key and the download matches it.</p>
      <button onclick="App.updSave()">Save</button></div></div>`;
}

// ------------------------------------------------------------------- start

// ------------------------------------------------------------------ agents

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const gb = (b) => (b ? (b / 1073741824).toFixed(b >= 10737418240 ? 0 : 1) + ' GB' : '0');
const uptimeFmt = (s) => (s ? (s >= 86400 ? Math.floor(s / 86400) + 'd ' : '') + Math.floor((s % 86400) / 3600) + 'h ' + Math.floor((s % 3600) / 60) + 'm' : '—');

function meter(pct, label) {
  pct = Math.max(0, Math.min(100, Math.round(pct || 0)));
  const col = pct > 90 ? 'var(--bad)' : pct > 75 ? '#d97706' : 'var(--ok)';
  return `<div class="meter" title="${esc(label || '')}"><span style="width:${pct}%;background:${col}"></span></div>`;
}

/// Enqueue a job for an agent and wait for its result.
async function runJob(id, kind, params = {}) {
  const { job_id } = await POST(`/agents/${id}/jobs`, { kind, params });
  for (let i = 0; i < 90; i++) {
    const j = await GET('/jobs/' + job_id);
    if (j.status !== 'queued' && j.status !== 'running') return j;
    await sleep(800);
  }
  throw new Error('the agent did not return a result in time (is it online?)');
}

function runAsControls(p) {
  return `<select id="${p}-mode" onchange="App.runAsVis('${p}')">
      <option value="system">As SYSTEM (the agent)</option>
      <option value="loggedon">As the logged-on user</option>
      <option value="user">As a specific account…</option></select>
    <span id="${p}-extra" style="display:none;gap:6px">
      <input id="${p}-user" autocomplete="off" placeholder="CORP\\user or .\\localuser" style="max-width:190px">
      <input id="${p}-pass" type="password" autocomplete="new-password" placeholder="password" style="max-width:150px"></span>`;
}
function readRunAs(p) {
  const m = $('#' + p + '-mode').value;
  if (m === 'loggedon') return { impersonate: true };
  if (m === 'user') return { user: $('#' + p + '-user').value.trim(), password: $('#' + p + '-pass').value };
  return {};
}

async function pageAgents(el) {
  const agents = await GET('/agents');
  el.innerHTML = `
    <div class="page-head"><h1>Agents</h1><span class="sub">${agents.length} enrolled, ${agents.filter((a) => a.online).length} online</span>
      <div class="actions"><button onclick="location.hash='#/settings'">Enroll an agent</button></div></div>
    ${agents.length ? `<div class="card"><div class="card-body flush"><div class="table-wrap"><table>
      <thead><tr><th>Computer</th><th>Status</th><th>CPU</th><th>Memory</th><th>Disk</th><th>Version</th><th>Last seen</th></tr></thead><tbody>
      ${agents.map((a) => {
        const worst = (a.disks || []).reduce((m, d) => Math.max(m, d.total ? 100 * (d.total - d.available) / d.total : 0), 0);
        const memPct = a.mem_total ? 100 * a.mem_used / a.mem_total : 0;
        return `<tr class="click" onclick="location.hash='#/agent/${a.id}'">
          <td><b>${esc(a.computer)}</b><div class="os">${osLabel(a.os)}</div></td>
          <td>${pill(a.online ? 'online' : 'offline')}</td>
          <td style="min-width:90px">${meter(a.cpu_pct, 'CPU')}<span class="small muted">${a.cpu_pct != null ? Math.round(a.cpu_pct) + '%' : '—'}</span></td>
          <td style="min-width:90px">${meter(memPct, 'Memory')}<span class="small muted">${gb(a.mem_used)} / ${gb(a.mem_total)}</span></td>
          <td style="min-width:90px">${meter(worst, 'Disk')}<span class="small muted">${Math.round(worst)}%</span></td>
          <td class="small">${esc(a.version || '—')}</td>
          <td class="small muted" title="${fmtTime(a.last_checkin)}">${a.last_checkin ? ago(a.last_checkin) : 'never'}</td></tr>`;
      }).join('')}</tbody></table></div></div></div>`
      : `<div class="card"><div class="card-body"><div class="empty"><b>No agents yet</b>
        Agents let you manage computers that have no WinRM or SSH — they connect out to this console.
        <div style="margin-top:10px"><a href="#/settings"><b>Settings → Agents</b></a> has the one-line installer and enrollment token.</div></div></div></div>`}`;
  poll(() => pageAgents(el).catch(() => {}), 10000);
}

let ASEL = 'overview'; // selected agent detail tab

async function pageAgent(el, id) {
  const d = await GET('/agents/' + id);
  const a = d.agent;
  const tabs = [['overview', 'Overview'], ['eventlog', 'Event Log'], ['tasks', 'Scheduled Tasks'], ['firewall', 'Firewall'], ['users', 'Local Users'], ['terminal', 'Terminal']];
  el.innerHTML = `
    <div class="page-head"><h1>${esc(a.computer)}</h1>${pill(a.online ? 'online' : 'offline')}
      <span class="sub">${osLabel(a.os)} · ${esc(a.os_version || '')} · agent ${esc(a.version || '?')}</span>
      <div class="actions"><button class="danger" onclick="App.agentRemove('${a.id}')">Remove agent</button></div></div>
    <div class="tabs">${tabs.map(([k, l]) => `<button class="tab ${k === ASEL ? 'on' : ''}" onclick="App.aTab('${k}')">${l}</button>`).join('')}</div>
    <div id="atab"></div>`;
  const box = $('#atab');
  if (ASEL === 'overview') overviewTab(box, d);
  else if (ASEL === 'eventlog') eventlogTab(box, d);
  else if (ASEL === 'tasks') tasksTab(box, d);
  else if (ASEL === 'firewall') firewallTab(box, d);
  else if (ASEL === 'users') usersTab(box, d);
  else if (ASEL === 'terminal') terminalTab(box, a);
  if (ASEL === 'overview' && a.online) poll(() => pageAgent(el, id).catch(() => {}), 8000);
}

function syncedLine(inv, label, onclick) {
  return `<div class="card-head"><h2>${label}</h2><div class="right">
    ${inv && inv.synced_at ? `<span class="small muted">synced ${ago(inv.synced_at)}</span>` : ''}
    <button class="primary" onclick="${onclick}">${inv && inv.synced_at ? 'Re-sync' : 'Sync now'}</button></div></div>`;
}

function overviewTab(box, d) {
  const a = d.agent;
  const memPct = a.mem_total ? 100 * a.mem_used / a.mem_total : 0;
  box.innerHTML = `
    <div class="stats">
      <div class="stat"><div class="n">${a.cpu_pct != null ? Math.round(a.cpu_pct) + '%' : '—'}</div><div class="l">CPU${a.processes ? ' · ' + a.processes + ' processes' : ''}</div>${meter(a.cpu_pct, 'CPU')}</div>
      <div class="stat"><div class="n">${Math.round(memPct)}%</div><div class="l">Memory ${gb(a.mem_used)} / ${gb(a.mem_total)}</div>${meter(memPct, 'Memory')}</div>
      <div class="stat"><div class="n">${uptimeFmt(a.uptime_secs)}</div><div class="l">Uptime</div></div>
    </div>
    <div class="grid two">
      <div class="card"><div class="card-head"><h2>Disks</h2></div><div class="card-body">
        ${(a.disks || []).length ? (a.disks).map((dk) => {
          const pct = dk.total ? 100 * (dk.total - dk.available) / dk.total : 0;
          return `<div class="disk"><div class="disk-l"><b>${esc(dk.mount)}</b><span class="small muted">${gb(dk.total - dk.available)} used of ${gb(dk.total)} · ${gb(dk.available)} free</span></div>${meter(pct, dk.mount)}</div>`;
        }).join('') : '<span class="muted">No metrics yet.</span>'}</div></div>
      <div class="card"><div class="card-head"><h2>Run a command</h2></div><div class="card-body">
        <div class="row"><div class="field"><label>Shell</label><select id="c-shell"><option value="powershell">PowerShell</option><option value="cmd">cmd</option><option value="bash">bash</option></select></div>
          <div class="field"><label>Run as</label><div class="runas">${runAsControls('c')}</div></div></div>
        <textarea id="c-script" rows="3" placeholder="Get-Service | Where Status -eq Running"></textarea>
        <div style="margin-top:8px"><button class="primary" id="c-run" onclick="App.aRun('${a.id}')">Run</button></div>
        <pre class="log" id="c-out" style="min-height:0;margin-top:10px;display:none"></pre></div></div>
    </div>
    <div class="card"><div class="card-head"><h2>Recent jobs</h2></div><div class="card-body flush">
      ${d.jobs.length ? `<div class="table-wrap"><table><thead><tr><th>Job</th><th>Status</th><th>When</th><th>Result</th></tr></thead><tbody>
        ${d.jobs.map((j) => `<tr><td>${esc(j.kind)}</td><td>${pill(j.status)}</td><td class="small muted">${ago(j.finished_at || j.created_at)}</td>
          <td class="small">${esc((j.output || '').slice(0, 140))}</td></tr>`).join('')}</tbody></table></div>`
        : '<div class="empty">No jobs run yet.</div>'}</div></div>`;
}

function eventlogTab(box, d) {
  const inv = d.eventlog;
  const rows = (inv && Array.isArray(inv.data)) ? inv.data : (inv && inv.data ? [inv.data] : []);
  box.innerHTML = `
    <div class="card"><div class="card-head"><h2>Collect Windows Event Log</h2></div><div class="card-body">
      <div class="row">
        <div class="field"><label>Log</label><select id="ev-log">${['System', 'Application', 'Security', 'Setup'].map((l) => `<option>${l}</option>`).join('')}</select></div>
        <div class="field"><label>Level</label><select id="ev-level"><option value="">Any</option><option value="1">Critical</option><option value="2">Error</option><option value="3">Warning</option><option value="4">Information</option></select></div>
        <div class="field"><label>Last (hours)</label><input id="ev-hours" type="number" min="0" value="24"></div>
        <div class="field"><label>Max events</label><input id="ev-max" type="number" min="1" value="200"></div>
        <div class="field"><label>Event IDs (optional)</label><input id="ev-ids" placeholder="1000, 7034"></div>
      </div>
      <button class="primary" id="ev-run" onclick="App.aEventlog('${d.agent.id}')">Collect</button>
      ${inv && inv.synced_at ? `<span class="small muted" style="margin-left:10px">last collected ${ago(inv.synced_at)}</span>` : ''}</div></div>
    <div class="card"><div class="card-body flush">${rows.length ? `<div class="table-wrap"><table>
      <thead><tr><th>Time</th><th>Level</th><th>ID</th><th>Provider</th><th>Message</th></tr></thead><tbody>
      ${rows.map((e) => `<tr><td class="small" style="white-space:nowrap">${esc(fmtTime(e.time))}</td>
        <td>${pill(String(e.level || '').toLowerCase().includes('err') || String(e.level || '').toLowerCase().includes('crit') ? 'failed' : String(e.level || '').toLowerCase().includes('warn') ? 'scheduled' : 'success')}</td>
        <td class="num">${esc(e.Id ?? e.id ?? '')}</td><td class="small">${esc(e.provider || '')}</td>
        <td class="small">${esc((e.message || '').slice(0, 300))}</td></tr>`).join('')}</tbody></table></div>`
      : '<div class="empty">No events collected yet. Choose filters above and press Collect.</div>'}</div></div>`;
}

function tasksTab(box, d) {
  const inv = d.tasks;
  const rows = (inv && Array.isArray(inv.data)) ? inv.data : (inv && inv.data ? [inv.data] : []);
  const id = d.agent.id;
  box.innerHTML = `
    <div class="card">${syncedLine(inv, 'Scheduled Tasks', `App.aSync('${id}','tasks_list')`)}
      <div class="card-body flush">${rows.length ? `<div class="table-wrap"><table>
        <thead><tr><th>Task</th><th>State</th><th>Last run</th><th>Next run</th><th>Last result</th><th></th></tr></thead><tbody>
        ${rows.map((t) => `<tr><td><b>${esc(t.name)}</b><div class="os">${esc(t.path || '')}</div></td>
          <td>${pill(String(t.state).toLowerCase() === 'disabled' ? 'offline' : 'success')}</td>
          <td class="small muted">${esc(t.last_run || '')}</td><td class="small muted">${esc(t.next_run || '')}</td>
          <td class="num small">${esc(String(t.last_result ?? ''))}</td>
          <td class="rowbtns">
            <button onclick="App.aTask('${id}','run',${jattr(t)})">Run</button>
            ${String(t.state).toLowerCase() === 'disabled' ? `<button onclick="App.aTask('${id}','enable',${jattr(t)})">Enable</button>` : `<button onclick="App.aTask('${id}','disable',${jattr(t)})">Disable</button>`}
            <button class="danger" onclick="App.aTask('${id}','delete',${jattr(t)})">Delete</button></td></tr>`).join('')}
        </tbody></table></div>` : '<div class="empty">Press “Sync now” to read this computer’s scheduled tasks.</div>'}</div></div>`;
}

function firewallTab(box, d) {
  const inv = d.firewall;
  const rows = (inv && Array.isArray(inv.data)) ? inv.data : (inv && inv.data ? [inv.data] : []);
  const id = d.agent.id;
  box.innerHTML = `
    <div class="card">${syncedLine(inv, 'Windows Firewall rules', `App.aSync('${id}','firewall_list')`)}
      <div class="card-body"><button onclick="App.aFwAdd('${id}')">Add rule</button></div>
      <div class="card-body flush">${rows.length ? `<div class="table-wrap"><table>
        <thead><tr><th>Rule</th><th>Dir</th><th>Action</th><th>Profile</th><th>Enabled</th><th></th></tr></thead><tbody>
        ${rows.map((r) => `<tr><td><b>${esc(r.DisplayName || r.Name)}</b>${r.group ? `<div class="os">${esc(r.group)}</div>` : ''}</td>
          <td class="small">${esc(r.direction || '')}</td><td class="small">${esc(r.action || '')}</td><td class="small">${esc(r.profile || '')}</td>
          <td>${r.enabled ? pill('success') : pill('offline')}</td>
          <td class="rowbtns">
            ${r.enabled ? `<button onclick="App.aFw('${id}','disable',${jattr({ name: r.Name })})">Disable</button>` : `<button onclick="App.aFw('${id}','enable',${jattr({ name: r.Name })})">Enable</button>`}
            <button class="danger" onclick="App.aFw('${id}','delete',${jattr({ name: r.Name })})">Delete</button></td></tr>`).join('')}
        </tbody></table></div>` : '<div class="empty">Press “Sync now” to read the firewall rules.</div>'}</div></div>`;
}

function usersTab(box, d) {
  const inv = d.users;
  const data = inv && inv.data && !Array.isArray(inv.data) ? inv.data : {};
  const users = data.users || [];
  const groups = data.groups || [];
  const id = d.agent.id;
  box.innerHTML = `
    <div class="card">${syncedLine(inv, 'Local users & groups', `App.aSync('${id}','users_list')`)}
      <div class="card-body"><button onclick="App.aUserAdd('${id}')">Add user</button></div>
      <div class="card-body flush">${users.length ? `<div class="table-wrap"><table>
        <thead><tr><th>User</th><th>Full name</th><th>Enabled</th><th></th></tr></thead><tbody>
        ${users.map((u) => `<tr><td><b>${esc(u.Name)}</b>${u.Description ? `<div class="os">${esc(u.Description)}</div>` : ''}</td>
          <td class="small">${esc(u.full_name || '')}</td><td>${u.enabled ? pill('success') : pill('offline')}</td>
          <td class="rowbtns">
            <button onclick="App.aUserPw('${id}',${jattr({ user: u.Name })})">Set password</button>
            ${u.enabled ? `<button onclick="App.aUser('${id}','disable',${jattr({ user: u.Name })})">Disable</button>` : `<button onclick="App.aUser('${id}','enable',${jattr({ user: u.Name })})">Enable</button>`}
            <button onclick="App.aUserGroup('${id}',${jattr({ user: u.Name })})">Groups…</button>
            <button class="danger" onclick="App.aUser('${id}','delete',${jattr({ user: u.Name })})">Delete</button></td></tr>`).join('')}
        </tbody></table></div>` : '<div class="empty">Press “Sync now” to read local users.</div>'}
      ${groups.length ? `<div class="card-body"><h3 class="small" style="margin:0 0 6px">Groups</h3>
        ${groups.map((g) => `<div class="small"><b>${esc(g.name)}</b> <span class="muted">${esc((g.members || []).join(', ') || 'no members')}</span></div>`).join('')}</div>` : ''}</div></div>`;
}

let TERM = null; // active terminal WebSocket

function terminalTab(box, a) {
  box.innerHTML = `
    <div class="card"><div class="card-body">
      <div class="row">
        <div class="field"><label>Shell</label><select id="t-shell"><option value="powershell">PowerShell</option><option value="cmd">cmd</option>${a.os === 'linux' ? '<option value="bash">bash</option>' : ''}</select></div>
        <div class="field"><label>Run as</label><div class="runas">${runAsControls('t')}</div></div>
        <div class="field" style="align-self:end"><button class="primary" id="t-connect" onclick="App.termConnect('${a.id}')">Connect</button>
          <button id="t-close" onclick="App.termClose()" disabled>Close</button></div>
      </div>
      <pre class="term" id="t-out"></pre>
      <input id="t-in" class="term-in" placeholder="Connect, then type here and press Enter" disabled></div></div>`;
  const inp = $('#t-in');
  inp.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && TERM && TERM.ws.readyState === 1) {
      TERM.ws.send(JSON.stringify({ type: 'input', data: inp.value + '\n' }));
      $('#t-out').textContent += inp.value + '\n';
      inp.value = '';
      $('#t-out').scrollTop = $('#t-out').scrollHeight;
    }
  });
}

Object.assign(App, {
  aTab(k) { ASEL = k; route(); },
  runAsVis(p) { $('#' + p + '-extra').style.display = $('#' + p + '-mode').value === 'user' ? 'inline-flex' : 'none'; },
  agentRemove: act(async (id) => {
    if (!confirm('Remove this agent? The computer stays in the list; the agent will re-enroll if it is still running.')) return;
    await DEL('/agents/' + id);
    location.hash = '#/agents';
  }),
  aRun: act(async (id) => {
    const script = $('#c-script').value;
    if (!script.trim()) return;
    const btn = $('#c-run'); btn.disabled = true; btn.textContent = 'Running…';
    const out = $('#c-out'); out.style.display = 'block'; out.textContent = 'Running on the agent…';
    try {
      const j = await runJob(id, 'command', { shell: $('#c-shell').value, script, run_as: readRunAs('c') });
      out.textContent = j.output || '(no output)';
    } finally { btn.disabled = false; btn.textContent = 'Run'; }
  }),
  aEventlog: act(async (id) => {
    const btn = $('#ev-run'); btn.disabled = true; btn.textContent = 'Collecting…';
    const ids = $('#ev-ids').value.split(',').map((x) => parseInt(x.trim(), 10)).filter((n) => !isNaN(n));
    try {
      const j = await runJob(id, 'eventlog', {
        log: $('#ev-log').value, level: $('#ev-level').value ? +$('#ev-level').value : null,
        since_hours: +$('#ev-hours').value || null, max: +$('#ev-max').value || 200, event_ids: ids,
      });
      if (j.status !== 'success') throw new Error(j.output || 'collection failed');
      route();
    } catch (e) { btn.disabled = false; btn.textContent = 'Collect'; throw e; }
  }),
  aSync: act(async (id, kind) => {
    const j = await runJob(id, kind, {});
    if (j.status !== 'success') throw new Error(j.output || 'sync failed');
    toast('Synced'); route();
  }),
  aTask: act(async (id, action, t) => {
    t = App._p(t);
    if (action === 'delete' && !confirm(`Delete scheduled task ${t.name}?`)) return;
    const j = await runJob(id, 'task_control', { action, path: t.path, name: t.name });
    if (j.status !== 'success') throw new Error(j.output || 'failed');
    toast('Task ' + action); await App._reSync(id, 'tasks_list');
  }),
  aFw: act(async (id, action, r) => {
    r = App._p(r);
    if (action === 'delete' && !confirm(`Delete firewall rule?`)) return;
    const j = await runJob(id, 'firewall_set', { action, name: r.name });
    if (j.status !== 'success') throw new Error(j.output || 'failed');
    toast('Firewall ' + action); await App._reSync(id, 'firewall_list');
  }),
  aFwAdd(id) {
    modal('Add firewall rule', `
      <div class="field"><label>Display name</label><input id="fw-name" placeholder="Allow app 8080"></div>
      <div class="row">
        <div class="field"><label>Direction</label><select id="fw-dir"><option>Inbound</option><option>Outbound</option></select></div>
        <div class="field"><label>Action</label><select id="fw-act"><option>Allow</option><option>Block</option></select></div>
        <div class="field"><label>Protocol</label><select id="fw-proto"><option value="">Any</option><option>TCP</option><option>UDP</option></select></div>
      </div>
      <div class="row">
        <div class="field"><label>Local port(s)</label><input id="fw-port" placeholder="8080 or 80,443"></div>
        <div class="field"><label>Remote address</label><input id="fw-raddr" placeholder="any or 10.0.0.0/8"></div>
        <div class="field"><label>Program (optional)</label><input id="fw-prog" placeholder="C:\\app\\app.exe"></div>
      </div>`,
      [{ label: 'Cancel' }, { label: 'Add', cls: 'primary', onClick: async () => {
        const j = await runJob(id, 'firewall_set', {
          action: 'new', display_name: $('#fw-name').value.trim(), direction: $('#fw-dir').value, fw_action: $('#fw-act').value,
          protocol: $('#fw-proto').value, local_port: $('#fw-port').value.trim(), remote_address: $('#fw-raddr').value.trim(), program: $('#fw-prog').value.trim(),
        });
        if (j.status !== 'success') throw new Error(j.output || 'failed');
        toast('Rule added'); await App._reSync(id, 'firewall_list');
      } }]);
  },
  aUserAdd(id) {
    modal('Add local user', `
      <div class="field"><label>User name</label><input id="u-name" placeholder="svc-app"></div>
      <div class="field"><label>Full name (optional)</label><input id="u-full"></div>
      <div class="field"><label>Password</label><input id="u-pass" type="password" autocomplete="new-password"><div class="hint">Leave empty to create a no-password account.</div></div>`,
      [{ label: 'Cancel' }, { label: 'Create', cls: 'primary', onClick: async () => {
        const j = await runJob(id, 'user_control', { action: 'create', user: $('#u-name').value.trim(), full_name: $('#u-full').value.trim(), password: $('#u-pass').value || null });
        if (j.status !== 'success') throw new Error(j.output || 'failed');
        toast('User created'); await App._reSync(id, 'users_list');
      } }]);
  },
  aUserPw(id, u) {
    u = App._p(u);
    modal('Set password for ' + u.user, `<div class="field"><label>New password</label><input id="u-pw" type="password" autocomplete="new-password"></div>`,
      [{ label: 'Cancel' }, { label: 'Set', cls: 'primary', onClick: async () => {
        const j = await runJob(id, 'user_control', { action: 'setpassword', user: u.user, password: $('#u-pw').value });
        if (j.status !== 'success') throw new Error(j.output || 'failed');
        toast('Password set');
      } }]);
  },
  aUserGroup(id, u) {
    u = App._p(u);
    modal('Group membership for ' + u.user, `
      <div class="row"><div class="field"><label>Group</label><input id="g-name" placeholder="Administrators"></div>
        <div class="field" style="align-self:end"><label class="check"><input type="radio" name="g-act" value="addgroup" checked> Add</label>
          <label class="check"><input type="radio" name="g-act" value="removegroup"> Remove</label></div></div>`,
      [{ label: 'Cancel' }, { label: 'Apply', cls: 'primary', onClick: async () => {
        const action = $$('input[name=g-act]').find((r) => r.checked).value;
        const j = await runJob(id, 'user_control', { action, user: u.user, group: $('#g-name').value.trim() });
        if (j.status !== 'success') throw new Error(j.output || 'failed');
        toast('Group updated'); await App._reSync(id, 'users_list');
      } }]);
  },
  aUser: act(async (id, action, u) => {
    u = App._p(u);
    if (action === 'delete' && !confirm(`Delete local user ${u.user}?`)) return;
    const j = await runJob(id, 'user_control', { action, user: u.user });
    if (j.status !== 'success') throw new Error(j.output || 'failed');
    toast('User ' + action); await App._reSync(id, 'users_list');
  }),
  _p(v) { return typeof v === 'string' ? JSON.parse(v) : v; },
  async _reSync(id, kind) { try { await runJob(id, kind, {}); } catch { /* ignore */ } route(); },
  termConnect(id) {
    App.termClose();
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const url = `${proto}://${location.host}/api/agents/${id}/terminal${TOKEN ? '?token=' + encodeURIComponent(TOKEN) : ''}`;
    const out = $('#t-out');
    out.textContent = 'Connecting…\n';
    const ws = new WebSocket(url);
    TERM = { ws, id };
    ws.onopen = () => {
      ws.send(JSON.stringify({ shell: $('#t-shell').value, run_as: readRunAs('t') }));
      $('#t-in').disabled = false; $('#t-close').disabled = false; $('#t-connect').disabled = true;
      $('#t-in').focus();
    };
    ws.onmessage = (ev) => {
      let m; try { m = JSON.parse(ev.data); } catch { return; }
      if (m.type === 'output') { out.textContent += m.data; out.scrollTop = out.scrollHeight; }
      else if (m.type === 'ready') { out.textContent += '[connected]\n'; }
      else if (m.type === 'closed') { out.textContent += '\n[session closed]\n'; App.termClose(); }
    };
    ws.onclose = () => { if (TERM && TERM.ws === ws) App._termReset(); };
    ws.onerror = () => { out.textContent += '\n[connection error]\n'; };
  },
  termClose() { if (TERM) { try { TERM.ws.send(JSON.stringify({ type: 'close' })); TERM.ws.close(); } catch { /* */ } } App._termReset(); },
  _termReset() {
    TERM = null;
    const c = $('#t-connect'), cl = $('#t-close'), i = $('#t-in');
    if (c) c.disabled = false; if (cl) cl.disabled = true; if (i) i.disabled = true;
  },
});

/// JSON for an inline onclick attribute (single-quoted, HTML-escaped).
/** A string as a JS string literal that is safe inside a double-quoted HTML
 *  attribute (inline handlers). esc() alone is not enough there: the browser
 *  decodes &#39; back to ' before the handler's JavaScript is parsed. */
function jstr(s) {
  return esc(JSON.stringify(String(s ?? '')));
}

function jattr(obj) {
  return `'${esc(JSON.stringify(obj)).replace(/&#39;/g, '\\u0027')}'`;
}

// ----------------------------------------------------------- windows update

const WU_PKG = 'windows-update';
const UP = { tab: 'computers', qs: { computers: '', updates: '' }, filter: '', cls: '', declined: false, data: null };
const SEV_PILL = { Critical: 's-failed', Important: 's-partial', Moderate: 's-scheduled', Low: 's-skipped' };
const sevPill = (s) => (s ? `<span class="pill ${SEV_PILL[s] || ''}">${esc(s)}</span>` : '');
const WU_RESULT = { 2: ['installed', 's-success'], 3: ['with errors', 's-partial'], 4: ['failed', 's-failed'], 5: ['aborted', 's-cancelled'] };
const wuSize = (b) => (b ? sizeFmt(b) : '');
const WU_SIZE_HINT = 'The largest possible download; Windows usually downloads a much smaller delta';

function pollUpdates(el) {
  poll(async () => {
    try {
      const r = await GET('/updates');
      if (r.scan.running) {
        if ($('#up-progress')) $('#up-progress').innerHTML = scanProgress(r.scan);
        pollUpdates(el);
      } else {
        toast(`Update scan finished${r.scan.failed ? ` (${r.scan.failed} not scanned)` : ''}`);
        pageUpdates(el);
      }
    } catch { pollUpdates(el); }
  }, 3000);
}

async function pageUpdates(el) {
  const [r, computers] = await Promise.all([GET('/updates'), GET('/computers')]);
  const windows = computers.filter((c) => c.os !== 'linux');
  const st = Object.fromEntries(r.computers.map((s) => [s.computer.toLowerCase(), s]));
  const scanned = r.computers.filter((s) => s.scanned_at);
  UP.data = { r, windows, st };
  const n = {
    current: scanned.filter((s) => !s.missing).length,
    security: scanned.filter((s) => s.security).length,
    missing: scanned.filter((s) => s.missing).length,
    reboot: scanned.filter((s) => s.info.reboot_pending === 'true').length,
    failed: windows.filter((c) => { const s = st[c.name.toLowerCase()]; return !s || s.status !== 'ok'; }).length,
  };
  const s = r.settings;
  const stat = (key, num, label, color) => `<div class="stat click ${UP.filter === key ? 'on' : ''}" onclick="App.upFilter('${key}')">
    <div class="n" style="color:${num && color ? color : 'inherit'}">${num}</div><div class="l">${label}</div></div>`;
  el.innerHTML = `
    <div class="page-head"><h1>Updates</h1><span class="sub">Windows Update on ${scanned.length} of ${windows.length} Windows computers</span>
      <div class="actions"><button onclick="App.upSchedule()" title="Install updates every month, e.g. two days after Patch Tuesday">New patch schedule</button>
        <button class="primary" onclick="App.upScan()" ${r.scan.running || !windows.length ? 'disabled' : ''}>Scan all computers</button></div></div>
    <div id="up-progress">${scanProgress(r.scan)}</div>
    <div class="stats">
      ${stat('current', n.current, 'Up to date', 'var(--ok)')}
      ${stat('security', n.security, 'Missing security updates', 'var(--bad)')}
      ${stat('missing', n.missing, 'Missing any update', 'var(--warn)')}
      ${stat('reboot', n.reboot, 'Restart pending', 'var(--warn)')}
      ${stat('failed', n.failed, 'Not scanned or last scan failed', 'var(--bad)')}
    </div>
    <div class="card"><div class="card-head">
      <div class="tabs">${[['computers', 'Computers'], ['updates', `Missing updates (${r.updates.filter((u) => !u.declined).length})`]]
        .map(([k, l]) => `<button class="${UP.tab === k ? 'on' : ''}" onclick="App.upTab('${k}')">${l}</button>`).join('')}</div>
      <input class="search" id="up-q" placeholder="${UP.tab === 'computers' ? 'Search computer, OS build, update source…' : 'Search KB or title…'}" value="${esc(UP.qs[UP.tab])}" oninput="App.upSearch(this.value)">
      ${UP.tab === 'updates' ? `<select onchange="App.upClass(this.value)"><option value="">All classifications</option>
        ${r.classifications.map((c) => `<option ${UP.cls === c ? 'selected' : ''}>${esc(c)}</option>`).join('')}</select>
        <label class="check small"><input type="checkbox" ${UP.declined ? 'checked' : ''} onchange="App.upShowDeclined(this.checked)"> Show declined</label>` : ''}
      <div class="right" id="up-actions"></div>
    </div><div class="card-body flush" id="up-body"></div></div>
    <div class="card"><div class="card-head"><h2>Scan settings</h2>
      ${s.last_scan ? `<span class="small muted">Last full scan started ${ago(s.last_scan)}</span>` : ''}</div><div class="card-body">
      <div class="row">
        <div class="field"><label>Scan all Windows computers every (hours)</label><input id="up-int" type="number" min="0" value="${s.interval_hours}">
          <div class="hint">0 = only when you click Scan. 24 is a good start.</div></div>
        <div class="field"><label>Computers scanned at the same time</label><input id="up-conc" type="number" min="1" max="128" value="${s.concurrency}"></div>
        <div class="field"><label>Look for updates at</label><select id="up-src">
          <option value="default" ${s.source === 'default' ? 'selected' : ''}>The computer's own source (WSUS if set, else Windows Update)</option>
          <option value="windows-update" ${s.source === 'windows-update' ? 'selected' : ''}>Windows Update, ignoring WSUS</option></select></div>
      </div>
      <label class="check"><input type="checkbox" id="up-re" ${s.rescan_after_install ? 'checked' : ''}> Scan computers again after a deployment installs updates on them</label>
      <p class="small muted">A scan asks the Windows Update Agent which updates are missing, over WinRM with the computer's credential. Nothing is
        downloaded or installed. Because Windows does not let a remote session use Windows Update, the scan and the
        <b>Windows Update - install updates</b> package run as SYSTEM through a short-lived scheduled task. Install from here, from Deploy,
        or on a schedule. Use a batch size there to restart web farms one server at a time.</p>
      <button onclick="App.upSave()">Save</button></div></div>`;
  if (r.scan.running) pollUpdates(el);
  renderUpdatesBody();
}

function updateComputerRows() {
  const { windows, st } = UP.data;
  const q = UP.qs.computers.toLowerCase();
  return windows.map((c) => ({ c, s: st[c.name.toLowerCase()] })).filter(({ c, s }) => {
    const ok = s && s.scanned_at;
    if (UP.filter === 'current' && !(ok && !s.missing)) return false;
    if (UP.filter === 'security' && !(ok && s.security)) return false;
    if (UP.filter === 'missing' && !(ok && s.missing)) return false;
    if (UP.filter === 'reboot' && !(ok && s.info.reboot_pending === 'true')) return false;
    if (UP.filter === 'failed' && s && s.status === 'ok') return false;
    const i = (s && s.info) || {};
    return !q || [c.name, i.os_name, i.os_build, i.source].some((v) => (v || '').toLowerCase().includes(q));
  });
}

function updateRows() {
  const q = UP.qs.updates.toLowerCase();
  return UP.data.r.updates.filter((u) => (UP.declined || !u.declined) && (!UP.cls || u.update.classification === UP.cls)
    && (!q || u.key.toLowerCase().includes(q) || u.update.title.toLowerCase().includes(q)));
}

function renderUpdatesBody() {
  if (UP.tab === 'updates') return renderMissingUpdates();
  const rows = updateComputerRows();
  const todo = rows.filter(({ s }) => s && s.missing);
  $('#up-actions').innerHTML = `<button onclick="App.upExport()">Export CSV</button>
    <button class="primary" onclick="App.upInstallComputers()" ${todo.length ? '' : 'disabled'}
      title="Install the missing updates on the computers listed here">Install on ${todo.length} computer(s)…</button>`;
  $('#up-body').innerHTML = !UP.data.windows.length
    ? '<div class="empty"><b>No Windows computers yet</b>Add computers first, then scan them.</div>'
    : `<div class="table-wrap"><table><thead><tr><th>Computer</th><th>Operating system</th><th>Missing</th>
        <th>Last installed</th><th>Source</th><th>Scanned</th></tr></thead><tbody>
      ${rows.map(({ c, s }) => {
        const ok = s && s.scanned_at;
        const i = (s && s.info) || {};
        const missing = !ok ? '' : !s.missing ? '<span class="pill s-success">up to date</span>'
          : `${s.security ? `<span class="pill s-failed">${s.security} security</span> ` : ''}<b>${s.missing}</b> missing`;
        return `<tr class="click" onclick="App.upShow(${jstr(c.name)})">
          <td><b>${esc(c.name)}</b>${i.reboot_pending === 'true' ? ' <span class="pill s-partial">restart pending</span>' : ''}</td>
          <td>${ok ? `${esc(i.os_name || 'Windows')}<div class="os">${esc(i.os_build ? 'build ' + i.os_build : '')}</div>` : '<span class="muted">Windows</span>'}</td>
          <td>${missing}${ok && (s.optional || s.declined) ? `<div class="small muted">${[s.optional && `${s.optional} optional`, s.declined && `${s.declined} declined`].filter(Boolean).join(' · ')}</div>` : ''}</td>
          <td class="small" title="${esc(fmtTime(i.last_install))}">${i.last_install ? ago(i.last_install) : ''}</td>
          <td class="small">${esc(i.source || '')}</td>
          <td class="small">${!s ? '<span class="muted">never</span>'
            : `${ok ? ago(s.scanned_at) : '<span class="muted">never</span>'}${s.status !== 'ok' ? `<div class="err" title="${esc(s.error || '')}">${esc(s.status)} ${ago(s.last_attempt)}</div>` : ''}`}</td></tr>`;
      }).join('')}</tbody></table></div>
      ${rows.length ? '' : '<div class="empty">No computers match.</div>'}`;
}

function renderMissingUpdates() {
  const rows = updateRows();
  $('#up-actions').innerHTML = `<button onclick="App.upExport()">Export CSV</button>
    <button onclick="App.upDecline(true)" title="Never install these and don't count them as missing">Decline</button>
    ${UP.declined ? '<button onclick="App.upDecline(false)">Undecline</button>' : ''}
    <button class="primary" onclick="App.upInstallSelected()">Install selected…</button>`;
  $('#up-body').innerHTML = rows.length ? `<div class="table-wrap"><table><thead><tr>
      <th style="width:30px"><input type="checkbox" onclick="$$('input[data-k]', this.closest('table')).forEach((i) => { i.checked = this.checked; })"></th>
      <th>Update</th><th>Classification</th><th>Severity</th><th title="${WU_SIZE_HINT}">Max size</th><th>Released</th><th class="num">Computers</th></tr></thead><tbody>
    ${rows.map((u) => {
      const x = u.update;
      return `<tr><td><input type="checkbox" data-k="${esc(u.key)}"></td>
        <td><b>${esc(x.kb || '')}</b> ${esc(x.title)}${u.declined ? ' <span class="tag">declined</span>' : ''}${x.optional ? ' <span class="tag">optional</span>' : ''}
          ${x.reboot === 1 ? '<span class="tag" title="Needs a restart">restart</span>' : ''}</td>
        <td class="small">${esc(x.classification)}</td><td>${sevPill(x.severity)}</td>
        <td class="small">${wuSize(x.size)}</td><td class="small">${esc(x.released)}</td>
        <td class="num" title="${esc(u.computers.join(', '))}">${u.computers.length}</td></tr>`;
    }).join('')}</tbody></table></div>`
    : `<div class="empty">${UP.data.r.updates.length ? 'No updates match.' : '<b>Nothing missing</b>Scan the computers to see which updates they need.'}</div>`;
}

const pickedUpdates = () => $$('input[data-k]:checked', $('#up-body')).map((i) => UP.data.r.updates.find((u) => u.key === i.dataset.k)).filter(Boolean);

Object.assign(App, {
  upTab(t) { UP.tab = t; route(); },
  upFilter(f) { UP.filter = UP.filter === f ? '' : f; UP.tab = 'computers'; route(); },
  upSearch(v) { UP.qs[UP.tab] = v; renderUpdatesBody(); },
  upClass(c) { UP.cls = c; renderUpdatesBody(); },
  upShowDeclined(on) { UP.declined = on; renderUpdatesBody(); },
  upScan: act(async (names) => {
    const r = await POST('/updates/scan', { names: names || null });
    toast(`Scanning ${r.started} computer(s) for updates…`);
    route();
  }),
  upSave: act(async () => {
    await PUT('/settings/windows-update', {
      interval_hours: +$('#up-int').value || 0, concurrency: +$('#up-conc').value || 16, source: $('#up-src').value,
      rescan_after_install: $('#up-re').checked,
    });
    toast('Saved');
  }),
  upDecline: act(async (declined) => {
    const picked = pickedUpdates();
    if (!picked.length) throw new Error('Tick the updates first');
    await POST('/updates/decline', { keys: picked.map((u) => u.key), declined });
    toast(declined ? `Declined ${picked.length} update(s): PSCCM will not install them` : `${picked.length} update(s) no longer declined`);
    route();
  }),
  upInstallSelected: act(async () => {
    const picked = pickedUpdates().filter((u) => !u.declined);
    if (!picked.length) throw new Error('Tick the updates to install (declined ones are never installed)');
    const computers = [...new Set(picked.flatMap((u) => u.computers))];
    await deployPrefilled(WU_PKG, computers, { kbs: picked.map((u) => u.key).join(', ') });
  }),
  upInstallComputers: act(async () => {
    const names = updateComputerRows().filter(({ s }) => s && s.missing).map(({ c }) => c.name);
    await deployPrefilled(WU_PKG, names);
  }),
  upSchedule: act(async () => {
    const { packages } = await loadPackages(true);
    if (!packages.some((p) => p.id === WU_PKG)) throw new Error(`The package '${WU_PKG}' is missing (deleted or disabled)`);
    await App.editSchedule(0, {
      name: 'Monthly Windows updates', package_id: WU_PKG, kind: 'monthly', time: '02:00', weekdays: [1], week: 2, offset_days: 2,
      options: { concurrency: 10, batch_size: 0, max_failures: null, use_detect: false },
    });
  }),
  upExport() {
    if (UP.tab === 'computers') {
      downloadCsv('update-status', ['Computer', 'OS', 'Build', 'Missing', 'Security', 'Optional', 'Declined', 'Restart pending', 'Last installed', 'Source', 'Scanned', 'Status', 'Error'],
        updateComputerRows().map(({ c, s }) => {
          const i = (s && s.info) || {};
          return [c.name, i.os_name, i.os_build, s ? s.missing : '', s ? s.security : '', s ? s.optional : '', s ? s.declined : '',
            i.reboot_pending, fmtTime(i.last_install), i.source, s ? fmtTime(s.scanned_at) : '', s ? s.status : 'never scanned', (s && s.error) || ''];
        }));
    } else {
      downloadCsv('missing-updates', ['KB', 'Title', 'Classification', 'Severity', 'Max size bytes', 'Released', 'Declined', 'Optional', 'Computer'],
        updateRows().flatMap((u) => u.computers.map((c) => [u.update.kb, u.update.title, u.update.classification, u.update.severity,
          u.update.size, u.update.released, u.declined ? 'yes' : '', u.update.optional ? 'yes' : '', c])));
    }
  },
  upShow: act(async (name) => {
    const r = await GET('/updates/computer/' + encodeURIComponent(name)).catch(() => null);
    const scanBtn = { label: 'Scan now', onClick: async () => { await App.upScan([name]); } };
    if (!r) {
      modal(name, '<p class="muted">This computer has not been scanned for updates yet.</p>', [{ label: 'Close' }, { ...scanBtn, cls: 'primary' }]);
      return;
    }
    const { status: s, missing } = r;
    const declined = new Set(r.declined.map((d) => d.toLowerCase()));
    const key = (u) => (u.kb || u.update_id).toLowerCase();
    const i = s.info;
    const facts = [['OS', i.os_name], ['Build', i.os_build], ['Update source', i.source], ['Restart pending', i.reboot_pending === 'true' ? 'yes' : 'no'],
      ['Last install', i.last_install ? `${fmtTime(i.last_install)} (${ago(i.last_install)})` : '']].filter(([, v]) => v);
    modal(name, `
      <div class="small muted" style="margin-bottom:10px">${s.scanned_at ? `Scanned ${fmtTime(s.scanned_at)} (${ago(s.scanned_at)})` : 'Never scanned successfully'}
        ${s.status !== 'ok' ? `<div class="err">Last attempt ${ago(s.last_attempt)}: ${esc(s.status)} — ${esc(s.error || '')}</div>` : ''}</div>
      <dl class="facts">${facts.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>
      <div class="cat">Missing updates (${missing.length})</div>
      ${missing.length ? `<div class="table-wrap" style="max-height:40vh;overflow:auto"><table><thead><tr><th style="width:30px"></th><th>Update</th><th>Classification</th><th>Severity</th><th title="${WU_SIZE_HINT}">Max size</th></tr></thead><tbody>
        ${missing.map((u) => {
          const d = declined.has(key(u));
          return `<tr><td><input type="checkbox" data-u="${esc(u.kb || u.update_id)}" ${d || u.optional ? '' : 'checked'} ${d ? 'disabled' : ''}></td>
            <td><b>${esc(u.kb)}</b> ${esc(u.title)}${d ? ' <span class="tag">declined</span>' : ''}${u.optional ? ' <span class="tag">optional</span>' : ''}</td>
            <td class="small">${esc(u.classification)}</td><td>${sevPill(u.severity)}</td><td class="small">${wuSize(u.size)}</td></tr>`;
        }).join('')}</tbody></table></div>` : '<div class="small muted">Nothing missing.</div>'}
      <div class="cat">Recent installs</div>
      ${s.history.length ? `<div class="table-wrap" style="max-height:30vh;overflow:auto"><table><tbody>
        ${s.history.map((h) => {
          const [label, cls] = WU_RESULT[h.result] || ['unknown', ''];
          return `<tr><td class="small" title="${esc(fmtTime(h.at))}">${ago(h.at)}</td><td><span class="pill ${cls}">${label}</span></td><td>${esc(h.title)}</td></tr>`;
        }).join('')}</tbody></table></div>` : '<div class="small muted">No installs recorded.</div>'}`,
    [{ label: 'Close' }, scanBtn, ...(missing.length ? [{
      label: 'Install selected…', cls: 'primary', onClick: async (root) => {
        const kbs = $$('input[data-u]:checked', root).map((c) => c.dataset.u);
        if (!kbs.length) throw new Error('Tick the updates to install');
        await deployPrefilled(WU_PKG, [name], { kbs: kbs.join(', ') });
      },
    }] : [])], { wide: true });
  }),
});

const PAGES = {
  dashboard: pageDashboard, deploy: pageDeploy, history: pageHistory, deployment: pageDeployment,
  computers: pageComputers, packages: pagePackages, package: pagePackage, schedules: pageSchedules, settings: pageSettings,
  gpo: pageGpo, agents: pageAgents, agent: pageAgent, inventory: pageInventory, catalog: pageCatalog, updates: pageUpdates,
};

route();
loadUpdateBadge();
