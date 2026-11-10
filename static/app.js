'use strict';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

/** Tiny DOM builder. Text children are always inserted as text (no HTML injection). */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v == null) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'text') el.textContent = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function show(el, on = true) { (typeof el === 'string' ? $(el) : el).classList.toggle('hidden', !on); }

async function api(method, path, body) {
  const opt = { method, headers: { 'X-Requested-With': 'kaffeknap' }, credentials: 'same-origin' };
  if (body !== undefined) {
    opt.headers['Content-Type'] = 'application/json';
    opt.body = JSON.stringify(body);
  }
  const res = await fetch('/api' + path, opt);
  let data = null;
  try { data = await res.json(); } catch { /* empty body */ }
  if (!res.ok) {
    const err = new Error((data && data.error) || res.statusText || 'Request failed');
    err.status = res.status;
    throw err;
  }
  return data;
}

let toastTimer;
function toast(msg) {
  const t = $('#toast');
  t.textContent = msg;
  show(t);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => show(t, false), 3200);
}

function fail(err) {
  if (err && err.status === 401) { showAuth(); return; }
  console.error(err);
  toast(err && err.message ? err.message : 'Something went wrong');
}

const fmtTime = (ts) => new Date(ts * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
const fmtDate = (ts) => new Date(ts * 1000).toLocaleDateString([], { weekday: 'short', day: 'numeric', month: 'short' });
function fmtLeft(secs) {
  secs = Math.max(0, Math.round(secs));
  const m = Math.floor(secs / 60), s = secs % 60;
  return `${m}:${String(s).padStart(2, '0')}`;
}
function storage(key, value) {
  try {
    if (value === undefined) return localStorage.getItem(key);
    if (value === null) localStorage.removeItem(key); else localStorage.setItem(key, value);
  } catch { return null; }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

const S = {
  me: null,
  groups: [],
  pending: [],
  active: null,      // active group id
  gstate: null,      // coffee state for active group
  group: null,       // group detail (incl. invite code)
  members: [],
  requests: [],
  prefs: null,
  history: [],
  historyDone: false,
  offset: 0,         // server clock - local clock (seconds)
  tab: 'button',
  authMode: 'login',
  joinCode: null,
  events: null,      // EventSource
  pushSub: null,
};

const serverNow = () => Date.now() / 1000 + S.offset;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

function applyTheme() {
  const choice = storage('kaffe-theme') || 'system';
  if (choice === 'system') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.setAttribute('data-theme', choice);
  const dark = choice === 'dark' || (choice === 'system' && matchMedia('(prefers-color-scheme: dark)').matches);
  $('#theme-toggle').textContent = dark ? '☀️' : '🌙';
  $$('#theme-seg button').forEach(b => b.classList.toggle('active', b.dataset.themeChoice === choice));
}
function setTheme(choice) { storage('kaffe-theme', choice); applyTheme(); }
$('#theme-toggle').addEventListener('click', () => {
  const dark = document.documentElement.getAttribute('data-theme') === 'dark' ||
    (!document.documentElement.getAttribute('data-theme') && matchMedia('(prefers-color-scheme: dark)').matches);
  setTheme(dark ? 'light' : 'dark');
});
$$('#theme-seg button').forEach(b => b.addEventListener('click', () => setTheme(b.dataset.themeChoice)));
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', applyTheme);

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

function showView(name) {
  for (const v of ['auth', 'nogroup', 'join', 'main']) show('#view-' + v, v === name);
  show('#group-select', name === 'main' && S.groups.length > 0);
}

function setTab(tab) {
  S.tab = tab;
  $$('.main-tabs .tab').forEach(t => t.classList.toggle('active', t.dataset.tab === tab));
  for (const p of ['button', 'history', 'group', 'me']) show('#tab-' + p, p === tab);
  if (tab === 'history') loadHistory(true).catch(fail);
  if (tab === 'group') loadGroupDetails().catch(fail);
  if (tab === 'me') renderPushStatus();
  storage('kaffe-tab', tab);
}
$$('.main-tabs .tab').forEach(t => t.addEventListener('click', () => setTab(t.dataset.tab)));

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

function setAuthMode(mode) {
  S.authMode = mode;
  $$('[data-auth-tab]').forEach(t => t.classList.toggle('active', t.dataset.authTab === mode));
  $$('.signup-only').forEach(el => show(el, mode === 'signup'));
  const form = $('#auth-form');
  form.password.autocomplete = mode === 'signup' ? 'new-password' : 'current-password';
  $('#auth-submit').textContent = mode === 'signup' ? 'Create account' : 'Log in';
  show('#auth-error', false);
}
$$('[data-auth-tab]').forEach(t => t.addEventListener('click', () => setAuthMode(t.dataset.authTab)));

function showAuth() {
  S.me = null;
  if (S.events) { S.events.close(); S.events = null; }
  showView('auth');
  const note = $('#auth-invite-note');
  note.textContent = S.joinCode ? 'Log in or sign up to join the coffee group you were invited to.' : '';
  show(note, !!S.joinCode);
  updateTitle();
}

$('#auth-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = e.target;
  const errEl = $('#auth-error');
  show(errEl, false);
  if (S.authMode === 'signup' && f.password.value !== f.password2.value) {
    errEl.textContent = "Passwords don't match";
    show(errEl);
    return;
  }
  const btn = $('#auth-submit');
  btn.disabled = true;
  // Ask for notification permission while we still have the click (browsers
  // only show the prompt in response to a user gesture).
  const isSignup = S.authMode === 'signup';
  const permission = isSignup && pushSupported() && Notification.permission === 'default'
    ? Notification.requestPermission().catch(() => 'default')
    : null;
  try {
    S.me = await api('POST', S.authMode === 'signup' ? '/auth/signup' : '/auth/login', {
      initials: f.initials.value,
      password: f.password.value,
    });
    f.reset();
    await boot();
    if (permission && (await permission) === 'granted') await enablePush();
  } catch (err) {
    errEl.textContent = err.message;
    show(errEl);
  } finally {
    btn.disabled = false;
  }
});

$('#logout').addEventListener('click', async () => {
  try {
    await disablePush(true);
    await api('POST', '/auth/logout');
  } catch (err) { console.error(err); }
  location.href = '/';
});

$('#password-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = e.target, errEl = $('#password-error');
  show(errEl, false);
  if (f.new_password.value !== f.new_password2.value) {
    errEl.textContent = "New passwords don't match";
    show(errEl);
    return;
  }
  try {
    await api('POST', '/auth/password', { current_password: f.current_password.value, new_password: f.new_password.value });
    f.reset();
    toast('Password changed');
  } catch (err) {
    errEl.textContent = err.message;
    show(errEl);
  }
});

// ---------------------------------------------------------------------------
// Boot & data loading
// ---------------------------------------------------------------------------

async function boot() {
  $$('.me-initials').forEach(el => { el.textContent = S.me.initials; });
  $('#password-form').username.value = S.me.initials;
  connectEvents();
  syncPushSubscription().catch(console.error);
  await loadGroups();
  if (S.joinCode) return showJoin();
  await route();
}

async function route() {
  if (S.groups.length === 0) {
    S.active = null;
    renderPending();
    showView('nogroup');
    updateTitle();
    return;
  }
  if (!S.groups.some(g => g.id === S.me.active_group_id)) {
    const id = S.groups[0].id;
    await api('PUT', '/me/active-group', { group_id: id });
    S.me.active_group_id = id;
  }
  if (S.active !== S.me.active_group_id) {
    S.active = S.me.active_group_id;
    S.history = []; S.historyDone = false;
    await loadActive();
  } else {
    await refreshState();
  }
  renderGroupSelect();
  showView('main');
}

async function loadGroups() {
  const data = await api('GET', '/groups');
  S.groups = data.groups;
  S.pending = data.pending;
  renderGroupSelect();
  renderPending();
}

async function loadActive() {
  const id = S.active;
  const [group, prefs] = await Promise.all([
    api('GET', `/groups/${id}`),
    api('GET', `/groups/${id}/notifications`),
  ]);
  S.group = group;
  S.prefs = prefs;
  await refreshState();
  renderPrefs();
  setTab(S.tab); // also loads tab-specific data
}

async function refreshState() {
  if (!S.active) return;
  const prev = S.gstate;
  const st = await api('GET', `/groups/${S.active}/state`);
  if (st.group_id !== S.active) return;
  S.offset = st.server_time - Date.now() / 1000;
  S.gstate = st;
  renderState();
  localNotify(prev, st);
  // A break that just got completed shows up in history.
  if (prev && prev.live_event && !st.live_event && S.tab === 'history') loadHistory(true).catch(fail);
}

async function loadGroupDetails() {
  if (!S.active) return;
  const id = S.active;
  const [group, members] = await Promise.all([api('GET', `/groups/${id}`), api('GET', `/groups/${id}/members`)]);
  S.group = group;
  S.members = members;
  S.requests = group.is_owner ? await api('GET', `/groups/${id}/requests`) : [];
  renderGroupTab();
}

async function loadHistory(reset) {
  if (!S.active) return;
  if (reset) { S.history = []; S.historyDone = false; }
  const before = S.history.length ? S.history[S.history.length - 1].started_at : '';
  const rows = await api('GET', `/groups/${S.active}/history?limit=25${before ? '&before=' + before : ''}`);
  S.history.push(...rows);
  S.historyDone = rows.length < 25;
  renderHistory();
}

// ---------------------------------------------------------------------------
// Live updates (SSE)
// ---------------------------------------------------------------------------

function connectEvents() {
  if (S.events) S.events.close();
  const es = new EventSource('/api/stream');
  S.events = es;
  let first = true;
  es.addEventListener('hello', () => {
    if (!first) fullRefresh(); // reconnected: we may have missed something
    first = false;
  });
  es.addEventListener('group', (e) => {
    const { group_id } = JSON.parse(e.data);
    if (group_id === S.active) {
      refreshState().catch(fail);
      if (S.tab === 'group') loadGroupDetails().catch(fail);
    }
    loadGroups().catch(fail);
  });
  es.addEventListener('user', fullRefresh);
  es.addEventListener('resync', fullRefresh);
  es.onerror = async () => {
    // EventSource reconnects by itself; if the session is gone, go to login.
    if (es.readyState === EventSource.CLOSED) {
      try { await api('GET', '/me'); setTimeout(connectEvents, 3000); } catch (err) { fail(err); }
    }
  };
}

async function fullRefresh() {
  try {
    S.me = await api('GET', '/me');
    await loadGroups();
    if (!$('#view-join').classList.contains('hidden')) return;
    await route();
  } catch (err) { fail(err); }
}

document.addEventListener('visibilitychange', () => {
  if (!document.hidden && S.me && S.active) refreshState().catch(fail);
});

// ---------------------------------------------------------------------------
// Rendering: the button
// ---------------------------------------------------------------------------

const RING = 640.88;

function readyNow(st) {
  const t = serverNow();
  return st.ready.filter(r => r.expires_at > t);
}

function renderState() {
  const st = S.gstate;
  if (!st) return;
  const t = serverNow();
  const ready = readyNow(st);
  const live = st.live_event && st.live_event.expires_at > t ? st.live_event : null;
  const meReady = ready.some(r => r.user_id === S.me.id);
  const btn = $('#big-button'), label = $('#big-label'), sub = $('#big-sub');
  const ring = $('#ring-fg');

  btn.classList.remove('is-ready', 'is-live', 'joined');
  ring.classList.toggle('live', !!live);
  btn.disabled = false;

  const actions = $('#actions');
  actions.replaceChildren();

  if (live) {
    ring.style.strokeDashoffset = '0';
    btn.classList.add('is-live');
    if (live.i_participate) {
      btn.classList.add('joined');
      btn.disabled = true;
      label.textContent = 'Coffee time!';
      sub.textContent = `${live.participants.length} going`;
      actions.append(
        h('button', { class: 'btn success', type: 'button', onclick: completeEvent }, '✓ We had coffee'),
        h('button', { class: 'btn subtle', type: 'button', onclick: retract }, "I'm out"),
      );
    } else {
      label.textContent = 'Join break';
      sub.textContent = `${live.participants.length} going`;
    }
    $('#status-line').textContent = '☕ The coffee break is on!';
  } else {
    const n = ready.length, min = st.min_ready;
    ring.style.strokeDashoffset = String(RING * (1 - Math.min(n / min, 1)));
    if (meReady) {
      const mine = ready.find(r => r.user_id === S.me.id);
      btn.classList.add('is-ready');
      label.textContent = 'Ready ✓';
      sub.textContent = `${fmtLeft(mine.expires_at - t)} · tap to extend`;
      actions.append(h('button', { class: 'btn', type: 'button', onclick: retract }, 'Not ready anymore'));
    } else {
      label.textContent = 'Coffee?';
      sub.textContent = "Tap when you're ready";
    }
    const need = Math.max(min - n, 0);
    $('#status-line').textContent = `${n} / ${min} ready` + (need > 0 ? ` · ${need} more needed` : '');
  }

  // Live break details
  const lc = $('#live-card');
  if (live) {
    lc.replaceChildren(
      h('h3', {}, '☕ Coffee break'),
      h('p', { class: 'muted small' },
        `Started ${fmtTime(live.started_at)}`,
        live.started_by ? ` · first ready: ${live.started_by}` : '',
        live.triggered_by && live.triggered_by !== live.started_by ? ` · final press: ${live.triggered_by}` : ''),
      h('ul', { class: 'chips' }, live.participants.map(p =>
        h('li', { class: 'chip' + (p.user_id === S.me.id ? ' me' : '') }, p.initials,
          p.late_join ? h('span', { class: 'late' }, 'joined late') : null))),
      h('p', { class: 'muted small' },
        `Confirm within ${fmtLeft(live.expires_at - t)} to save it in history.`),
    );
    show(lc);
  } else {
    show(lc, false);
  }

  // Ready list
  const list = $('#ready-list');
  list.replaceChildren(...ready.map(r =>
    h('li', { class: 'chip' + (r.user_id === S.me.id ? ' me' : '') }, r.initials,
      h('span', { class: 't' }, fmtLeft(r.expires_at - t)))));
  $('#ready-count').textContent = live ? '' : `${ready.length}/${st.min_ready}`;
  show('#ready-empty', ready.length === 0);
  show('#ready-card', !live);

  $$('.group-name').forEach(el => { el.textContent = st.name; });
  updateTitle();
}

async function press() {
  if (!S.active) return;
  if (navigator.vibrate) navigator.vibrate(30);
  try {
    S.gstate = await api('POST', `/groups/${S.active}/ready`);
    S.offset = S.gstate.server_time - Date.now() / 1000;
    renderState();
    maybeSuggestPush();
  } catch (err) { fail(err); }
}
async function retract() {
  try {
    S.gstate = await api('DELETE', `/groups/${S.active}/ready`);
    renderState();
  } catch (err) { fail(err); }
}
async function completeEvent() {
  try {
    S.gstate = await api('POST', `/groups/${S.active}/event/complete`);
    renderState();
    toast('Saved to history ☕');
  } catch (err) { fail(err); }
}
$('#big-button').addEventListener('click', press);

// Tick countdowns.
setInterval(() => { if (S.gstate && !$('#view-main').classList.contains('hidden')) renderState(); }, 1000);

// ---------------------------------------------------------------------------
// Title & favicon reflect the active group's status
// ---------------------------------------------------------------------------

let lastFavicon = '';
function updateTitle() {
  const st = S.gstate;
  if (!S.me || !st || !S.active || $('#view-main').classList.contains('hidden')) {
    document.title = 'Kaffeknap';
    setFavicon(null);
    return;
  }
  const t = serverNow();
  const live = st.live_event && st.live_event.expires_at > t;
  const ready = readyNow(st);
  const meReady = ready.some(r => r.user_id === S.me.id);
  if (live) {
    document.title = `☕ Coffee time! · ${st.name}`;
    setFavicon({ text: '☕', color: '#2e7d32' });
  } else {
    document.title = `${ready.length}/${st.min_ready} ready${meReady ? ' ✓' : ''} · ${st.name}`;
    setFavicon({ text: String(ready.length), color: '#d32f2f' });
  }
}

function setFavicon(spec) {
  const key = spec ? spec.text + spec.color : '';
  if (key === lastFavicon) return;
  lastFavicon = key;
  const link = $('#favicon');
  if (!spec) { link.href = '/icon.svg'; return; }
  const c = document.createElement('canvas');
  c.width = c.height = 64;
  const ctx = c.getContext('2d');
  ctx.fillStyle = spec.color;
  ctx.beginPath(); ctx.arc(32, 32, 30, 0, Math.PI * 2); ctx.fill();
  ctx.fillStyle = '#fff';
  ctx.font = 'bold 38px system-ui, sans-serif';
  ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
  ctx.fillText(spec.text, 32, 35);
  link.href = c.toDataURL('image/png');
}

// ---------------------------------------------------------------------------
// Groups: selector, create, join, settings
// ---------------------------------------------------------------------------

function renderGroupSelect() {
  const sel = $('#group-select');
  sel.replaceChildren(...S.groups.map(g => {
    const status = g.live_event ? ' ☕' : g.ready_count ? ` (${g.ready_count}/${g.min_ready})` : '';
    return h('option', { value: g.id, selected: g.id === S.active }, g.name + status);
  }));
  const owned = S.groups.find(g => g.id === S.active);
  const badge = $('#req-badge');
  badge.textContent = owned && owned.pending_requests ? owned.pending_requests : '';
  show(badge, !!(owned && owned.pending_requests));
}

$('#group-select').addEventListener('change', (e) => switchGroup(Number(e.target.value)));

async function switchGroup(id) {
  try {
    await api('PUT', '/me/active-group', { group_id: id });
    S.me.active_group_id = id;
    S.gstate = null;
    await route();
    toast(`Switched to ${S.groups.find(g => g.id === id)?.name ?? 'group'}`);
  } catch (err) { fail(err); }
}

async function createGroup(e) {
  e.preventDefault();
  const name = e.target.name.value;
  try {
    const { id } = await api('POST', '/groups', { name });
    e.target.reset();
    S.me.active_group_id = id;
    await loadGroups();
    S.active = null;
    await route();
    setTab('group');
    toast('Group created. Share the invite link!');
  } catch (err) { fail(err); }
}
$('#create-group-form').addEventListener('submit', createGroup);
$('#create-group-form-2').addEventListener('submit', createGroup);

function parseInviteCode(input) {
  const s = input.trim();
  const m = s.match(/\/join\/([A-Za-z0-9]+)/);
  return (m ? m[1] : s).toLowerCase();
}
$$('.join-code-form').forEach(f => f.addEventListener('submit', (e) => {
  e.preventDefault();
  S.joinCode = parseInviteCode(e.target.code.value);
  e.target.reset();
  if (S.joinCode) showJoin();
}));

function renderPending() {
  const box = $('#pending-list');
  box.replaceChildren();
  if (!S.pending.length) return;
  box.append(h('p', { class: 'muted small' }, 'Waiting for approval:'),
    h('ul', { class: 'list' }, S.pending.map(p => h('li', {},
      h('span', { class: 'grow' }, p.name),
      h('button', { class: 'btn small subtle', type: 'button', onclick: () => cancelRequest(p.invite_code) }, 'Cancel')))));
}

async function cancelRequest(code) {
  try {
    await api('DELETE', `/invite/${encodeURIComponent(code)}/request`);
    await loadGroups();
    if (S.joinCode) showJoin(); else await route();
  } catch (err) { fail(err); }
}

async function showJoin() {
  const card = $('#join-card');
  card.replaceChildren(h('p', { class: 'muted' }, 'Loading invite…'));
  showView('join');
  const done = () => { S.joinCode = null; history.replaceState(null, '', '/'); };
  const back = h('button', { class: 'btn subtle', type: 'button', onclick: async () => { done(); await route(); } }, 'Back');
  let info;
  try {
    info = await api('GET', `/invite/${encodeURIComponent(S.joinCode)}`);
  } catch (err) {
    if (err.status === 401) return showAuth();
    card.replaceChildren(h('h2', {}, 'Invite not found'), h('p', { class: 'muted' }, err.message), back);
    return;
  }
  const title = h('h2', {}, info.name);
  const meta = h('p', { class: 'muted' }, `${info.member_count} member${info.member_count === 1 ? '' : 's'}`);
  if (info.status === 'member') {
    card.replaceChildren(title, meta, h('p', {}, "You're already a member."),
      h('button', { class: 'btn primary', type: 'button', onclick: async () => { done(); await switchGroup(info.group_id); } }, 'Open group'));
  } else if (info.status === 'pending') {
    card.replaceChildren(title, meta, h('p', {}, 'Your request is waiting for the owner to approve it.'),
      h('div', { class: 'row' },
        h('button', { class: 'btn', type: 'button', onclick: () => cancelRequest(S.joinCode) }, 'Cancel request'), back));
  } else {
    const joinBtn = h('button', { class: 'btn primary', type: 'button' }, info.is_open ? 'Join group' : 'Ask to join');
    joinBtn.addEventListener('click', async () => {
      joinBtn.disabled = true;
      try {
        const res = await api('POST', `/invite/${encodeURIComponent(S.joinCode)}/join`);
        await loadGroups();
        if (res.status === 'member') {
          done();
          S.me.active_group_id = res.group_id;
          S.active = null;
          await route();
          toast(`Welcome to ${info.name}!`);
        } else {
          showJoin();
        }
      } catch (err) { joinBtn.disabled = false; fail(err); }
    });
    card.replaceChildren(title, meta,
      h('p', {}, info.is_open ? "You've been invited to this coffee group." : 'The owner needs to approve new members.'),
      h('div', { class: 'row' }, joinBtn, back));
  }
}

function renderGroupTab() {
  const g = S.group;
  if (!g) return;
  const owner = g.is_owner;
  $$('.owner-only').forEach(el => show(el, owner));
  $$('.not-owner').forEach(el => show(el, !owner));
  show('.member-only-settings', !owner);

  $('#invite-link').value = `${location.origin}/join/${g.invite_code}`;
  $('#invite-note').textContent = g.is_open
    ? 'Anyone with this link can join.'
    : 'People with this link can ask to join. The owner approves requests.';

  if (owner) {
    const f = $('#group-settings');
    if (!f.contains(document.activeElement)) {
      f.name.value = g.name;
      f.min_ready.value = g.min_ready;
      f.press_ttl_minutes.value = g.press_ttl_minutes;
      f.event_ttl_minutes.value = g.event_ttl_minutes;
      f.is_open.checked = !!g.is_open;
    }
    $('#requests-list').replaceChildren(...S.requests.map(r => h('li', {},
      h('span', { class: 'grow' }, h('strong', {}, r.initials), ' ', h('span', { class: 'muted small' }, fmtDate(r.created_at))),
      h('button', { class: 'btn small success', type: 'button', onclick: () => decide(r.user_id, 'approve') }, 'Approve'),
      h('button', { class: 'btn small subtle', type: 'button', onclick: () => decide(r.user_id, 'reject') }, 'Reject'))));
    show('#requests-empty', S.requests.length === 0);
  } else {
    $('#settings-readonly').replaceChildren(
      h('dt', {}, 'People needed'), h('dd', {}, g.min_ready),
      h('dt', {}, 'Press valid for'), h('dd', {}, `${g.press_ttl_minutes} min`),
      h('dt', {}, 'Break stays live'), h('dd', {}, `${g.event_ttl_minutes} min`),
      h('dt', {}, 'Joining'), h('dd', {}, g.is_open ? 'Open' : 'Approval'),
    );
  }

  $('#member-count').textContent = `(${S.members.length})`;
  $('#members-list').replaceChildren(...S.members.map(m => h('li', {},
    h('span', { class: 'grow' }, h('strong', {}, m.initials), m.user_id === S.me.id ? ' (you)' : ''),
    m.is_owner ? h('span', { class: 'tag' }, 'owner') : null,
    owner && !m.is_owner
      ? h('button', { class: 'btn small subtle', type: 'button', onclick: () => removeMember(m) }, 'Remove')
      : null)));

  $('#groups-list').replaceChildren(...S.groups.map(gr => h('li', {},
    h('span', { class: 'grow' }, gr.name),
    gr.is_owner ? h('span', { class: 'tag' }, 'owner') : null,
    gr.id === S.active
      ? h('span', { class: 'tag' }, 'active')
      : h('button', { class: 'btn small', type: 'button', onclick: () => switchGroup(gr.id) }, 'Switch'))));
}

$('#group-settings').addEventListener('submit', async (e) => {
  e.preventDefault();
  const f = e.target, errEl = $('#settings-error');
  show(errEl, false);
  try {
    S.group = await api('PATCH', `/groups/${S.active}`, {
      name: f.name.value,
      min_ready: Number(f.min_ready.value),
      press_ttl_minutes: Number(f.press_ttl_minutes.value),
      event_ttl_minutes: Number(f.event_ttl_minutes.value),
      is_open: f.is_open.checked,
    });
    toast('Settings saved');
    await loadGroups();
    renderGroupTab();
  } catch (err) {
    errEl.textContent = err.message;
    show(errEl);
  }
});

$('#copy-invite').addEventListener('click', async () => {
  const input = $('#invite-link');
  try {
    if (navigator.share && matchMedia('(pointer: coarse)').matches) {
      await navigator.share({ title: `Join ${S.group.name} on Kaffeknap`, url: input.value });
      return;
    }
    await navigator.clipboard.writeText(input.value);
    toast('Invite link copied');
  } catch {
    input.select();
    document.execCommand && document.execCommand('copy');
  }
});

$('#regen-invite').addEventListener('click', async () => {
  if (!confirm('Create a new invite link? The current link will stop working.')) return;
  try {
    await api('POST', `/groups/${S.active}/invite-code`);
    await loadGroupDetails();
    toast('New invite link created');
  } catch (err) { fail(err); }
});

async function decide(uid, action) {
  try {
    await api('POST', `/groups/${S.active}/requests/${uid}/${action}`);
    await Promise.all([loadGroupDetails(), loadGroups()]);
  } catch (err) { fail(err); }
}

async function removeMember(m) {
  if (!confirm(`Remove ${m.initials} from the group?`)) return;
  try {
    await api('DELETE', `/groups/${S.active}/members/${m.user_id}`);
    await loadGroupDetails();
  } catch (err) { fail(err); }
}

$('#leave-group').addEventListener('click', async () => {
  if (!confirm(`Leave ${S.group.name}?`)) return;
  try {
    await api('DELETE', `/groups/${S.active}/members/${S.me.id}`);
    S.me = await api('GET', '/me');
    S.active = null; S.gstate = null;
    await loadGroups();
    await route();
    setTab('button');
  } catch (err) { fail(err); }
});

$('#delete-group').addEventListener('click', async () => {
  const name = S.group.name;
  const typed = prompt(`This deletes "${name}" including its history for everyone. Type the group name to confirm.`);
  if (typed === null) return;
  if (typed.trim() !== name) { toast("Name didn't match. Nothing deleted."); return; }
  try {
    await api('DELETE', `/groups/${S.active}`);
    S.me = await api('GET', '/me');
    S.active = null; S.gstate = null;
    await loadGroups();
    await route();
    setTab('button');
  } catch (err) { fail(err); }
});

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

function renderHistory() {
  const list = $('#history-list');
  list.replaceChildren(...S.history.map(ev => {
    const mins = ev.completed_at ? Math.max(1, Math.round((ev.completed_at - ev.started_at) / 60)) : null;
    return h('li', {},
      h('div', { class: 'when' }, `${fmtDate(ev.started_at)} · ${fmtTime(ev.started_at)}`),
      h('div', { class: 'meta' },
        ev.started_by ? `Started by ${ev.started_by}` : 'Started',
        ev.triggered_by && ev.triggered_by !== ev.started_by ? ` · final press ${ev.triggered_by}` : '',
        ev.completed_by ? ` · confirmed by ${ev.completed_by}` : '',
        mins ? ` after ${mins} min` : ''),
      h('ul', { class: 'chips' }, ev.participants.map(p =>
        h('li', { class: 'chip' + (p.initials === S.me.initials ? ' me' : '') }, p.initials,
          p.late_join ? h('span', { class: 'late' }, 'late') : null))));
  }));
  show('#history-empty', S.history.length === 0);
  show('#history-more', !S.historyDone && S.history.length > 0);
}
$('#history-more').addEventListener('click', () => loadHistory(false).catch(fail));

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

function renderPrefs() {
  const f = $('#notify-form');
  if (!S.prefs) return;
  for (const k of ['notify_press', 'notify_event_any', 'notify_event_mine']) f[k].checked = !!S.prefs[k];
}

$('#notify-form').addEventListener('change', async (e) => {
  const f = e.currentTarget;
  const body = {
    notify_press: f.notify_press.checked,
    notify_event_any: f.notify_event_any.checked,
    notify_event_mine: f.notify_event_mine.checked,
  };
  try {
    S.prefs = await api('PUT', `/groups/${S.active}/notifications`, body);
    toast('Notification settings saved');
    if (!S.pushSub && !('Notification' in window && Notification.permission === 'granted')) maybeSuggestPush(true);
  } catch (err) { fail(err); renderPrefs(); }
});

const pushSupported = () =>
  'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window && window.isSecureContext;

function b64ToBytes(b64) {
  const pad = '='.repeat((4 - (b64.length % 4)) % 4);
  const raw = atob((b64 + pad).replace(/-/g, '+').replace(/_/g, '/'));
  return Uint8Array.from(raw, c => c.charCodeAt(0));
}

async function swReady() {
  return navigator.serviceWorker.ready;
}

/** Re-registers an existing browser subscription with the server for this user. */
async function syncPushSubscription() {
  if (!pushSupported()) return renderPushStatus();
  const reg = await swReady();
  const sub = await reg.pushManager.getSubscription();
  S.pushSub = sub;
  if (sub && Notification.permission === 'granted') await api('POST', '/push/subscribe', sub.toJSON());
  renderPushStatus();
}

async function enablePush() {
  if (!pushSupported()) { renderPushStatus(); return; }
  const perm = await Notification.requestPermission();
  if (perm !== 'granted') {
    toast('Notifications are blocked. Allow them in your browser settings.');
    renderPushStatus();
    return;
  }
  try {
    const { public_key } = await api('GET', '/push/key');
    const reg = await swReady();
    let sub = await reg.pushManager.getSubscription();
    if (!sub) {
      sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64ToBytes(public_key) });
    }
    await api('POST', '/push/subscribe', sub.toJSON());
    S.pushSub = sub;
    toast('Notifications enabled on this device');
  } catch (err) { fail(err); }
  renderPushStatus();
}

async function disablePush(silent) {
  if (!pushSupported()) return;
  const reg = await swReady();
  const sub = await reg.pushManager.getSubscription();
  if (sub) {
    try { await api('POST', '/push/unsubscribe', { endpoint: sub.endpoint }); } catch (err) { console.error(err); }
    await sub.unsubscribe();
  }
  S.pushSub = null;
  if (!silent) { toast('Notifications disabled on this device'); renderPushStatus(); }
}

function renderPushStatus() {
  const el = $('#push-status');
  const on = !!S.pushSub && 'Notification' in window && Notification.permission === 'granted';
  show('#push-enable', !on);
  show('#push-disable', on);
  show('#push-test', on);
  if (!window.isSecureContext) {
    el.textContent = 'Push notifications need HTTPS. Open the app via https:// (or localhost).';
    show('#push-enable', false);
  } else if (!pushSupported()) {
    const ios = /iPad|iPhone|iPod/.test(navigator.userAgent);
    el.textContent = ios
      ? 'On iPhone/iPad: tap Share → "Add to Home Screen", open Kaffeknap from there, then enable notifications.'
      : "This browser doesn't support push notifications.";
    show('#push-enable', false);
  } else if (Notification.permission === 'denied') {
    el.textContent = 'Notifications are blocked for this site. Allow them in your browser settings.';
  } else if (on) {
    el.textContent = 'Enabled on this device. You get notified even when the tab is in the background or closed.';
  } else {
    el.textContent = 'Get notified even when the tab is in the background or closed.';
  }
}

$('#push-enable').addEventListener('click', enablePush);
$('#push-disable').addEventListener('click', () => disablePush(false).catch(fail));
$('#push-test').addEventListener('click', async () => {
  try { await api('POST', '/push/test'); toast('Test sent'); } catch (err) { fail(err); }
});

let suggested = false;
function maybeSuggestPush(force) {
  if ((suggested && !force) || !pushSupported() || S.pushSub || Notification.permission === 'denied') return;
  suggested = true;
  toast('Tip: enable notifications under "Me" to hear when it\'s coffee time');
}

/** "A", "A and B", "A, B and C". */
function listNames(names) {
  if (names.length <= 1) return names.join('');
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`;
}

/**
 * Fallback for browsers without a push subscription: if this tab is open but
 * hidden, show a notification for changes the user asked to hear about.
 */
function localNotify(prev, st) {
  if (!prev || prev.group_id !== st.group_id || !document.hidden || S.pushSub) return;
  if (!('Notification' in window) || Notification.permission !== 'granted' || !S.prefs) return;
  const notify = (title, body) => {
    try { new Notification(title, { body, tag: `kaffe-${st.group_id}`, icon: '/icon-192.png' }); } catch { /* mobile */ }
  };
  const ev = st.live_event;
  if (ev && (!prev.live_event || prev.live_event.id !== ev.id)) {
    const who = listNames(ev.participants.map(p => p.initials));
    const are = ev.participants.length === 1 ? 'is' : 'are';
    if (ev.i_participate && (S.prefs.notify_event_mine || S.prefs.notify_event_any)) {
      notify('☕ Coffee time!', `${who} ${are} ready. Let's go!`);
    } else if (!ev.i_participate && S.prefs.notify_event_any) {
      notify('☕ Coffee break started', `${who} ${are} heading for coffee. Join them!`);
    }
    return;
  }
  if (S.prefs.notify_press) {
    const before = new Set(prev.ready.map(r => r.user_id));
    const fresh = st.ready.filter(r => !before.has(r.user_id) && r.user_id !== S.me.id);
    if (fresh.length) {
      const are = fresh.length === 1 ? 'is' : 'are';
      notify('☕ Ready for coffee', `${listNames(fresh.map(r => r.initials))} ${are} ready for coffee (${st.ready.length}/${st.min_ready})`);
    }
  }
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

(async function start() {
  applyTheme();
  setAuthMode('login');
  const m = location.pathname.match(/^\/join\/([A-Za-z0-9]+)/);
  if (m) S.joinCode = m[1].toLowerCase();
  const savedTab = storage('kaffe-tab');
  if (['button', 'history', 'group', 'me'].includes(savedTab)) S.tab = savedTab;

  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/sw.js').catch(err => console.warn('SW registration failed', err));
  }

  try {
    S.me = await api('GET', '/me');
  } catch (err) {
    if (err.status === 401) return showAuth();
    return fail(err);
  }
  try { await boot(); } catch (err) { fail(err); }
})();
