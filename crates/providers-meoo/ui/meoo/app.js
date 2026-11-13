/**
 * Meoo 页面的交互：浏览器 OAuth 授权 / 手填 AK 兜底、单账号签到、模型清单。
 * 端点与凭证都在 api.js 里，这一层只管 DOM 与状态。
 */

import {
  addAccount,
  browserLoginCancel,
  browserLoginPoll,
  browserLoginReopen,
  browserLoginStart,
  deleteAccount,
  getModels,
  getSession,
  PROVIDER,
  refreshModels,
  requestTheme,
  runCheckin,
} from './api.js';
import {
  LOGIN_CANCELLED,
  LOGIN_PENDING,
  LOGIN_START,
  parseStart,
  signalOf,
} from './signal.js';

const $ = id => document.getElementById(id);

/* ─── 小工具 ────────────────────────────────────────────────── */

function toast(message, kind = 'ok', timeout = 5000) {
  const node = $('toast');
  node.textContent = message;
  node.dataset.kind = kind;
  node.hidden = false;
  window.clearTimeout(toast.timer);
  toast.timer = window.setTimeout(() => { node.hidden = true; }, timeout);
}

function busy(button, label) {
  if (label === undefined) {
    button.disabled = false;
    if (button.dataset.label) button.textContent = button.dataset.label;
    return;
  }
  if (!button.dataset.label) button.dataset.label = button.textContent;
  button.disabled = true;
  button.textContent = label;
}

/** `refreshedAt`（毫秒）→ `YYYY-MM-DD HH:mm`；缺失给空串 */
function formatTime(value) {
  const ms = Number(value);
  if (!Number.isFinite(ms) || ms <= 0) return '';
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return '';
  const pad = number => String(number).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} `
    + `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function accountsOf(session) {
  const raw = session?.accounts;
  const list = Array.isArray(raw) ? raw : Array.isArray(raw?.accounts) ? raw.accounts : [];
  return list.filter(item => item && item.provider === PROVIDER);
}

/** 网关批量签到的返回：`{ results: [{ ok, message, error, … }], … }` */
function checkinMessage(result) {
  const rows = Array.isArray(result?.results) ? result.results : [];
  const first = rows[0] || {};
  if (first.ok === false) {
    return { text: `签到失败：${first.message || first.error || '上游未接受'}`, bad: true };
  }
  const text = first.message || result?.message || '签到执行完成';
  return { text, bad: false };
}

/* ─── 账号列表 ──────────────────────────────────────────────── */

function renderAccounts(accounts) {
  const host = $('accounts');
  $('account-count').textContent = accounts.length ? `${accounts.length} 个` : '';
  if (!accounts.length) {
    host.innerHTML = '<p class="lx-empty">还没有账号。点上面的「打开浏览器登录」即可添加。</p>';
    return;
  }
  host.replaceChildren(...accounts.map(account => {
    const row = document.createElement('div');
    row.className = 'lx-row';

    const main = document.createElement('div');
    main.className = 'lx-row-main';
    const name = document.createElement('div');
    name.className = 'lx-row-name';
    name.textContent = account.name || account.nickname || account.id || 'Meoo 账号';
    main.append(name);

    const meta = document.createElement('div');
    meta.className = 'lx-row-meta';
    const facts = [
      account.uid ? `uid ${account.uid}` : '',
      account.tokenTail ? `····${account.tokenTail}` : '',
      account.hasOAuth === false ? '仅 AK（不可签到）' : '',
      account.enabled === false ? '已停用' : '',
    ].filter(Boolean);
    meta.textContent = facts.join(' · ') || '—';
    main.append(meta);

    const actions = document.createElement('div');
    actions.className = 'lx-row-actions';

    const sign = document.createElement('button');
    sign.className = 'lx-btn lx-btn-primary';
    sign.textContent = '签到';
    sign.addEventListener('click', async () => {
      busy(sign, '签到中…');
      try {
        const { text, bad } = checkinMessage(await runCheckin(account.id));
        toast(text, bad ? 'bad' : 'ok', 8000);
        await loadAccounts();
      } catch (error) {
        toast(String(error.message || error), 'bad');
      } finally {
        busy(sign);
      }
    });

    const remove = document.createElement('button');
    remove.className = 'lx-btn lx-btn-danger';
    remove.textContent = '删除';
    remove.addEventListener('click', async () => {
      if (!window.confirm(`删除「${name.textContent}」？账号里的凭证会一起删除。`)) return;
      busy(remove, '删除中…');
      try {
        await deleteAccount(account.id);
        toast('账号已删除');
        await Promise.all([loadAccounts(), loadModels()]);
      } catch (error) {
        busy(remove);
        toast(String(error.message || error), 'bad');
      }
    });

    actions.append(sign, remove);
    row.append(main, actions);
    return row;
  }));
}

async function loadAccounts() {
  try {
    const rows = accountsOf(await getSession());
    renderAccounts(rows);
    return rows;
  } catch (error) {
    $('accounts').innerHTML = '';
    $('account-count').textContent = '';
    toast(`读取账号失败：${error.message || error}`, 'bad');
    return null;
  }
}

/* ─── 模型清单 ──────────────────────────────────────────────── */

function renderModels(manage) {
  const host = $('models');
  const info = (Array.isArray(manage?.providers) ? manage.providers : [])
    .find(item => item && item.provider === PROVIDER);
  const count = Number(info?.count || 0);
  $('model-count').textContent = count ? `${count} 个` : '';

  const models = (Array.isArray(manage?.models) ? manage.models : [])
    .filter(item => item && (item.provider === undefined || item.provider === PROVIDER));

  if (!count || !models.length) {
    host.replaceChildren();
    $('model-hint').textContent = '还没有拉到目录。添加账号后点「刷新模型清单」。';
    return;
  }
  const refreshedAt = formatTime(info.refreshedAt);
  $('model-hint').textContent = `${info.source === 'remote' ? '上游目录' : '内置快照'}`
    + (refreshedAt ? ` · 更新于 ${refreshedAt}` : '');

  host.replaceChildren(...models.map(model => {
    const chip = document.createElement('code');
    chip.className = 'lx-chip';
    const id = model.id || model.name || '';
    chip.textContent = model.enabled === false ? `${id}（已停用）` : id;
    if (model.enabled === false) chip.dataset.off = '1';
    return chip;
  }));
}

async function loadModels() {
  try {
    renderModels(await getModels());
  } catch (error) {
    toast(`读取模型清单失败：${error.message || error}`, 'bad');
  }
}

async function refreshDirectory(quiet) {
  const button = $('refresh-models');
  if (!quiet) busy(button, '刷新中…');
  try {
    const result = await refreshModels();
    await loadModels();
    if (quiet) return;
    const failed = (Array.isArray(result?.results) ? result.results : [])
      .filter(item => item && item.status === 'failed');
    if (failed.length) {
      toast(`模型清单刷新失败：${failed[0].message || '原因未知'}`, 'bad');
    } else {
      toast('模型清单已刷新');
    }
  } catch (error) {
    if (!quiet) toast(`刷新模型清单失败：${error.message || error}`, 'bad');
  } finally {
    if (!quiet) busy(button);
  }
}

/* ─── 浏览器登录 ────────────────────────────────────────────── */

/**
 * 插件用 `account.normalize` 的带外前缀回报非终态：`start` 给出 flow id 与
 * 授权 URL，`poll` 在等待时给进度，只有真正登录后才会返回账号（网关随即落库）。
 * 这几个前缀不是错误，别拿去弹红色 toast。解析按**包含匹配**（见 signal.js）。
 *
 * ── 开窗由插件进程负责，页面只做兜底 ─────────────────────────
 * 页面在网关 webview / iframe 里，`window.open` 与 `<a target="_blank">`
 * 都可能被宿主策略拦掉（表现就是：流程在跑、一直「等待授权」，却没窗口）。
 * 所以 START 信令末位会带回「系统浏览器是否已拉起」：已拉起就关掉点击时
 * 预留的空白窗口，别重复开；没拉起才尝试 `window.open`。无论哪种结果，
 * 完整授权地址都明文摆在页面上，并提供「重新打开」与一键复制。
 */
const LOGIN_TIMEOUT_MS = 5 * 60 * 1000;

let loginFlow = '';
let loginTimer = 0;
let loginDeadline = 0;
let pendingWindow = null;

function reserveWindow() {
  try {
    pendingWindow = window.open('', '_blank');
    // 断开 opener，避免授权页通过 window.opener 反向操作本页。
    if (pendingWindow) {
      try { pendingWindow.opener = null; } catch { /* 部分浏览器不允许，忽略 */ }
    }
  } catch {
    pendingWindow = null;
  }
}

function closePendingWindow() {
  if (pendingWindow && !pendingWindow.closed) {
    try { pendingWindow.close(); } catch { /* 已跨域则关不掉，留着也无妨 */ }
  }
  pendingWindow = null;
}

function openAuthorize(url, openedByPlugin) {
  if (!url) {
    closePendingWindow();
    return;
  }
  if (openedByPlugin) {
    // 插件进程已调系统默认浏览器：关掉点击时预留的空白窗口，别开第二个。
    closePendingWindow();
  } else {
    try {
      if (pendingWindow && !pendingWindow.closed) {
        pendingWindow.location.replace(url);
      } else {
        window.open(url, '_blank', 'noopener,noreferrer');
      }
    } catch {
      try { window.open(url, '_blank'); } catch { /* 宿主禁弹窗，走手动兜底 */ }
    }
  }
  pendingWindow = null;
  $('authorize-url').href = url;
  $('authorize-text').textContent = url;
  $('authorize-link').hidden = false;
}

/** 剪贴板也不可用时，地址本身已经明文显示，用户仍可选中复制。 */
async function copyAuthorize() {
  const url = $('authorize-text').textContent.trim();
  if (!url) return;
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(url);
    } else {
      const area = document.createElement('textarea');
      area.value = url;
      area.style.position = 'fixed';
      area.style.opacity = '0';
      document.body.append(area);
      area.select();
      document.execCommand('copy');
      area.remove();
    }
    toast('授权地址已复制，粘贴到任意浏览器打开即可');
  } catch {
    toast('自动复制失败，请手动选中下面的地址复制', 'bad', 8000);
  }
}

/** 兜底链接：让插件进程再拉一次系统浏览器，而不是赌 webview 会放行弹窗。 */
async function reopenAuthorize(event) {
  event.preventDefault();
  const url = $('authorize-text').textContent.trim();
  if (!url) return;
  try {
    await browserLoginReopen(loginFlow);
    // 正常不会走到这里：插件借 PENDING 信令把结果放在错误文本里。
  } catch (error) {
    const text = String(error?.message || error);
    const hint = signalOf(text, LOGIN_PENDING);
    if (hint !== null) {
      toast(hint || '已重新尝试打开系统浏览器', 'ok', 7000);
      return;
    }
    toast(`重新打开失败：${text}`, 'bad');
  }
  try { window.open(url, '_blank', 'noopener,noreferrer'); } catch { /* 手动复制兜底已在页面上 */ }
}

function showLoginStatus(hint) {
  $('login-status').hidden = false;
  $('login-hint').textContent = hint || '等待你在浏览器里完成授权…';
  $('browser-login').disabled = true;
  $('browser-login-cancel').hidden = false;
}

function endLogin(message, kind) {
  window.clearTimeout(loginTimer);
  loginTimer = 0;
  loginFlow = '';
  closePendingWindow();
  $('authorize-link').hidden = true;
  $('login-status').hidden = true;
  $('browser-login-cancel').hidden = true;
  busy($('browser-login'));
  if (message) toast(message, kind || 'ok');
}

function scheduleLoginPoll(delay) {
  window.clearTimeout(loginTimer);
  loginTimer = window.setTimeout(() => { void pollBrowserLogin(); }, delay);
}

function beginLogin(flow, hint) {
  loginFlow = flow;
  loginDeadline = Date.now() + LOGIN_TIMEOUT_MS;
  showLoginStatus(hint || '已请求打开授权页，请在浏览器里登录并点「授权」…');
  scheduleLoginPoll(1600);
}

async function startBrowserLogin() {
  if (loginFlow) return;
  reserveWindow();
  busy($('browser-login'), '启动中…');
  try {
    const data = await browserLoginStart();
    // 正常路径不会走到这里：start 一定用带外信令回传 flow id。
    // 兼容「将来改成成功返回」的形态，别把 id 丢掉。
    const flow = String(data?.flow || '').trim();
    if (flow) {
      openAuthorize(String(data?.url || ''), false);
      beginLogin(flow, data?.instructions);
      return;
    }
    closePendingWindow();
    busy($('browser-login'));
    toast('登录流程已启动，但没拿到流程 id，请重试', 'bad');
  } catch (error) {
    const text = String(error?.message || error);
    const payload = signalOf(text, LOGIN_START);
    if (payload === null) {
      closePendingWindow();
      busy($('browser-login'));
      toast(text, 'bad');
      return;
    }
    const { flow, url, hint, opened } = parseStart(payload);
    if (!flow) {
      closePendingWindow();
      busy($('browser-login'));
      toast('登录流程 id 为空，请重试', 'bad');
      return;
    }
    openAuthorize(url, opened);
    // 开窗成功时给短提示（上方说明已经有完整指引）；失败时保留带原因的
    // 长提示，让用户立刻知道该去点页面上的授权链接。
    beginLogin(flow, opened
      ? '已请求系统默认浏览器打开授权页，请登录并点「授权」…'
      : hint);
  }
}

async function pollBrowserLogin() {
  if (!loginFlow) return;
  try {
    const data = await browserLoginPoll(loginFlow);
    const name = data?.account?.name || data?.name || '账号';
    endLogin();
    // 回查一次列表再报成功：插件返回账号 ≠ 网关一定落库（去重、账号库写失败
    // 都可能让列表还是空的）。没有就如实说，别刷一句假的「已添加」。
    const rows = await loadAccounts();
    await loadModels();
    refreshDirectory(true);
    if (rows && rows.length === 0) {
      toast('登录成功，但账号没有出现在列表里：请点右上「刷新」；仍为空请查看网关日志', 'bad', 9000);
      return;
    }
    toast(`✅ 已添加 ${name}`);
  } catch (error) {
    const text = String(error?.message || error);
    const pending = signalOf(text, LOGIN_PENDING);
    if (pending !== null) {
      if (Date.now() > loginDeadline) {
        const flow = loginFlow;
        endLogin('登录等待超时（5 分钟），已停止', 'bad');
        void browserLoginCancel(flow).catch(() => {});
        return;
      }
      showLoginStatus(pending || '等待你在浏览器里完成授权…');
      scheduleLoginPoll(2000);
      return;
    }
    if (signalOf(text, LOGIN_CANCELLED) !== null) {
      endLogin();
      return;
    }
    endLogin(`登录失败：${text}`, 'bad');
  }
}

async function cancelBrowserLogin() {
  const flow = loginFlow;
  endLogin('已取消登录');
  if (!flow) return;
  try {
    await browserLoginCancel(flow);
  } catch {
    /* 前端轮询已停；插件侧那轮 flow 会在 5 分钟后自行收摊 */
  }
}

/* ─── 添加账号（手动兜底） ──────────────────────────────────── */

async function addWithAk() {
  const button = $('add');
  const value = $('ak').value.trim();
  if (!value) {
    toast('先粘贴项目服务 AK 或凭证 JSON', 'bad');
    $('ak').focus();
    return;
  }
  busy(button, '添加中…');
  try {
    const data = await addAccount({ payload: value, name: $('name').value.trim() });
    toast(`✅ 已添加 ${data?.account?.name || '账号'}`);
    $('ak').value = '';
    $('name').value = '';
    await Promise.all([loadAccounts(), loadModels()]);
    refreshDirectory(true);
  } catch (error) {
    toast(String(error.message || error), 'bad');
  } finally {
    busy(button);
  }
}

/* ─── 装配 ──────────────────────────────────────────────────── */

requestTheme();
$('browser-login').addEventListener('click', () => { void startBrowserLogin(); });
$('browser-login-cancel').addEventListener('click', () => { void cancelBrowserLogin(); });
$('authorize-url').addEventListener('click', event => { void reopenAuthorize(event); });
$('copy-authorize').addEventListener('click', () => { void copyAuthorize(); });
$('add').addEventListener('click', addWithAk);
$('refresh-models').addEventListener('click', () => refreshDirectory(false));
$('reload').addEventListener('click', () => {
  void loadAccounts();
  void loadModels();
});
$('ak').addEventListener('keydown', event => {
  if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) void addWithAk();
});

void loadAccounts();
void loadModels();
