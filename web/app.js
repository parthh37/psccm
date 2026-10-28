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
  gpo: '<path d="M12 2l8 3v6c0 5-3.4 9.4-8 11-4.6-1.6-8-6-8-11V5zm0 2.2L6 6.4V11c0 3.9 2.5 7.4 6 8.9 3.5-1.5 6-5 6-8.9V6.4zM11 7h2v6h-2zm0 8h2v2h-2z"/>',
  schedules: '<path d="M7 2h2v2h6V2h2v2h3v18H4V4h3zm-1 8v10h12V10zm0-4v2h12V6z"/>',
  settings: '<path d="M12 8a4 4 0 1 1 0 8 4 4 0 0 1 0-8zm8.6 5.5l1.9 1.5-2 3.4-2.3-.9a7.7 7.7 0 0 1-2.1 1.2L15.8 21h-4l-.4-2.4a7.7 7.7 0 0 1-2.1-1.2l-2.3.9-2-3.4L7 13.4a7.8 7.8 0 0 1 0-2.8L5 9.1l2-3.4 2.3.9a7.7 7.7 0 0 1 2.1-1.2L11.8 3h4l.4 2.4a7.7 7.7 0 0 1 2.1 1.2l2.3-.9 2 3.4-1.9 1.5a7.8 7.8 0 0 1 0 2.9z"/>',
};

const NAV = [
  ['dashboard', 'Dashboard'], ['deploy', 'Deploy'], ['history', 'Deployments'], ['computers', 'Computers'],
  ['packages', 'Packages'], ['catalog', 'App catalog'], ['gpo', 'Group Policy'], ['schedules', 'Schedules'], ['settings', 'Settings'],
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
  const navKey = { deployment: 'history', package: 'packages' }[name] || name;
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
        ${CS.groups.map((x) => `<span class="chip ${CS.group === x.name ? 'on' : ''}" onclick="App.cGroup('${esc(x.name)}')" title="${esc(x.description)}">
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
        <td><input type="checkbox" ${CS.sel.has(c.name) ? 'checked' : ''} onchange="App.cSel('${esc(c.name)}', this.checked)"></td>
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
  editGroup(id) {
    const g = CS.groups.find((x) => x.id === id) || { id: 0, name: '', description: '', kind: 'static', rules: { tags: [] }, members: [] };
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
        route();
      },
    }]);
    const root = m.root;
    const rules = () => ({
      os: $('#gr-os').value || null, name: $('#gr-name').value.trim(), ou: $('#gr-ou').value.trim(),
      os_detail: $('#gr-osd').value.trim(), ad_group: $('#gr-ad').value.trim(),
      tags: $('#gr-tags').value.split(',').map((s) => s.trim()).filter(Boolean),
    });
    let timer;
    const preview = () => {
      clearTimeout(timer);
      timer = setTimeout(async () => {
        try {
          const r2 = await POST('/groups/preview', rules());
          $('#g-preview').innerHTML = r2.members.length
            ? `<b>${r2.members.length}</b> computer(s) match: <span class="muted">${esc(r2.members.slice(0, 12).join(', '))}${r2.members.length > 12 ? '…' : ''}</span>`
            : '<span class="muted">No computers match yet.</span>';
        } catch (e) { $('#g-preview').textContent = e.message; }
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
        ${list.map((p) => `<div class="pkg" onclick="location.hash='#/package/${esc(p.id)}'">
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
        <button class="danger" onclick="App.delPackage('${esc(p.id)}')">Delete</button>
        <button class="primary" onclick="App.deployPackage('${esc(p.id)}')">Deploy</button>
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
          <button class="primary" onclick="App.savePackage('${esc(p.id)}')">Save</button></div></div>
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
        ${ps.map((p) => `<div class="pkg ${W.pkg === p.id ? 'on' : ''}" onclick="App.wPick('${esc(p.id)}')">
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
      ${DEP.groups.map((g) => `<span class="chip ${W.groups.has(g.name) ? 'on' : ''}" onclick="App.wGroup('${esc(g.name)}')">${g.kind === 'dynamic' ? '⚡ ' : ''}${esc(g.name)} <span class="small">${g.members.length}</span></span>`).join('')}
    </div></div>` : ''}
    ${tags.length ? `<div class="field"><label>Tags <span class="muted small">(all computers with the tag, resolved when the deployment starts)</span></label><div class="chips">
      ${tags.map((t) => `<span class="chip ${W.tags.has(t) ? 'on' : ''}" onclick="App.wTag('${esc(t)}')">${esc(t)} <span class="small">${DEP.computers.filter((c) => c.tags.includes(t)).length}</span></span>`).join('')}
    </div></div>` : ''}
    <div class="field"><label>Computers</label>
      <div class="row" style="margin-bottom:8px"><input class="search" placeholder="Filter…" value="${esc(W.cq)}" oninput="App.wCq(this.value)" id="w-cq">
        <select style="width:auto" onchange="App.wSet('cos', this.value); renderWhere()">
          <option value="">All OS</option><option value="windows" ${W.cos === 'windows' ? 'selected' : ''}>Windows</option><option value="linux" ${W.cos === 'linux' ? 'selected' : ''}>Linux</option></select>
        <button onclick="App.wAll(true)">Select shown</button><button onclick="App.wAll(false)">Clear</button></div>
      ${DEP.computers.length ? `<div class="table-wrap" style="max-height:300px;overflow:auto;border:1px solid var(--border);border-radius:8px"><table><tbody>
        ${list.map((c) => `<tr class="click ${W.computers.has(c.name) ? 'sel' : ''}" onclick="App.wComp('${esc(c.name)}')">
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
          ? `<select onchange="App.wVar('${esc(k)}', this.value)">${v.options.map((o) => `<option ${o === val ? 'selected' : ''}>${esc(o)}</option>`).join('')}</select>`
          : `<input type="${v.secret ? 'password' : 'text'}" value="${esc(val)}" oninput="App.wVar('${esc(k)}', this.value)" ${v.required ? 'required' : ''}>`;
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

async function pageSchedules(el) {
  const [list, { packages }] = await Promise.all([GET('/schedules'), loadPackages(true)]);
  const pname = (id) => (packages.find((p) => p.id === id) || { name: id }).name;
  const when = (s) => s.kind === 'once' ? 'once at ' + s.time : s.kind === 'daily' ? 'daily at ' + s.time
    : s.kind === 'weekly' ? `${s.weekdays.map((d) => DAYS[d]).join(', ')} at ${s.time}` : `every ${s.interval_minutes} min`;
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
  async editSchedule(id) {
    const { packages } = await loadPackages();
    const [computers, gr] = await Promise.all([GET('/computers'), GET('/groups')]);
    const groupNames = gr.groups.map((g) => g.name);
    const tags = [...new Set(computers.flatMap((c) => c.tags))].sort();
    const s = (App._schedules || []).find((x) => x.id === id) || {
      id: 0, name: '', package_id: packages[0] && packages[0].id, kind: 'weekly', time: '02:00', weekdays: [5],
      interval_minutes: 60, enabled: true, targets: { computers: [], tags: [], groups: [] }, variables: {}, options: { concurrency: 10, batch_size: 0, max_failures: null, use_detect: true },
    };
    const m = modal(id ? 'Edit schedule' : 'New schedule', `
      <div class="row"><div class="field"><label>Name</label><input id="s-name" value="${esc(s.name)}" placeholder="Saturday patching"></div>
        <div class="field"><label>Package</label><select id="s-pkg">${packages.map((p) => `<option value="${esc(p.id)}" ${p.id === s.package_id ? 'selected' : ''}>${esc(p.name)}</option>`).join('')}</select></div></div>
      <div class="row"><div class="field"><label>Repeat</label><select id="s-kind">
          ${[['once', 'Once'], ['daily', 'Daily'], ['weekly', 'Weekly'], ['interval', 'Every N minutes']].map(([v, l]) => `<option value="${v}" ${s.kind === v ? 'selected' : ''}>${l}</option>`).join('')}</select></div>
        <div class="field" id="s-time-f"><label>Time</label><input id="s-time" value="${esc(s.time)}" placeholder="02:00"></div>
        <div class="field" id="s-int-f"><label>Minutes</label><input id="s-int" type="number" min="1" value="${s.interval_minutes}"></div></div>
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
          weekdays: $$('[data-day]', root).filter((c) => c.checked).map((c) => +c.dataset.day),
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
    ${updateCard(upd)}
    <div class="card"><div class="card-head"><h2>Folders</h2></div><div class="card-body small">
      <div>Data: <code>${esc(s.data_dir)}</code></div><div>Packages: <code>${esc(s.packages_dir)}</code></div><div>Repository: <code>${esc(s.repo_dir)}</code></div></div></div>`;
  const vis = () => { $('#ad-ldap').style.display = $('#ad-mode').value === 'ldap' ? '' : 'none'; };
  $('#ad-mode').onchange = vis;
  vis();
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
  forgetHost: act(async () => {
    const h = $('#kh-host').value.trim();
    if (!h) return;
    await DEL('/known-hosts/' + encodeURIComponent(h));
    toast('Forgot host key for ' + h);
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
            ${g.computer_version !== null ? `<button onclick="App.gpoImport('${esc(g.guid)}')">${m ? 'Re-import' : 'Import'}</button>` : ''}
            ${m ? `<button onclick="App.gpoView('${esc(g.guid)}')">View</button>
              <button class="primary" onclick="App.deployPackage('${esc(m.package_id)}')">Deploy</button>` : ''}
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
        ? `<button class="primary" onclick="App.catAdd('${esc(a.id)}')" ${a.busy ? 'disabled' : ''}>Add</button>`
        : `${a.update_available || !a.current || a.error ? `<button onclick="App.catUpdate('${esc(a.id)}')" ${a.busy ? 'disabled' : ''}>${a.current ? 'Update' : 'Retry'}</button>` : ''}
          <button class="primary" onclick="App.deployPackage('${esc(a.package_id)}')" ${a.current && !a.busy ? '' : 'disabled'}>Deploy</button>
          <button onclick="App.catGroups('${esc(a.id)}')">Auto-deploy</button>
          <button class="danger" onclick="App.catRemove('${esc(a.id)}')" ${a.busy ? 'disabled' : ''}>Remove</button>`}</div></td></tr>`;
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
        ${l && l.newer ? `<button class="primary" onclick="App.updInstall()">Install ${esc(l.version)}</button>` : ''}</div></div>
    <div class="card-body">
      ${l ? `<p style="margin-top:0">Latest release: <a href="${esc(l.url)}" target="_blank" rel="noopener">v${esc(l.version)}</a>
        ${l.newer ? pill('scheduled').replace('scheduled', 'update available') : pill('success').replace('success', 'up to date')}
        <span class="small muted">checked ${ago(u.settings.last_check)}</span></p>` : '<p class="muted" style="margin-top:0">Not checked yet.</p>'}
      ${u.platform === 'windows' && !u.installed ? '<p class="small muted">Running the portable exe: installing an update runs the setup program, which installs PSCCM as a service in Program Files.</p>' : ''}
      <label class="check"><input type="checkbox" id="u-check" ${u.settings.auto_check ? 'checked' : ''}> Check for updates daily</label>
      <label class="check"><input type="checkbox" id="u-install" ${u.settings.auto_install ? 'checked' : ''}> Install updates automatically (when no deployment is running)</label>
      <div class="row" style="margin-top:8px"><div class="field"><label>Release source (GitHub owner/repo)</label><input id="u-repo" value="${esc(u.settings.repo)}"></div></div>
      <p class="small muted">Downloads are verified against the release's SHA256SUMS before installing.</p>
      <button onclick="App.updSave()">Save</button></div></div>`;
}

// ------------------------------------------------------------------- start

const PAGES = {
  dashboard: pageDashboard, deploy: pageDeploy, history: pageHistory, deployment: pageDeployment,
  computers: pageComputers, packages: pagePackages, package: pagePackage, schedules: pageSchedules, settings: pageSettings,
  gpo: pageGpo, catalog: pageCatalog,
};

route();
loadUpdateBadge();
