/**
 * OpenCode 账号页的交互。
 *
 * 三块：添加账号（匿名免费 / 手工 Key）、账号列表、免费模型清单。
 * 端点与凭证都在 api.js 里，这一层只管 DOM 与状态。
 */

import {
  addAccount,
  addAnonymousAccount,
  addLocalAccount,
  deleteAccount,
  getModels,
  getSession,
  LOCAL_PROVIDER,
  PROVIDERS,
  refreshModels,
  requestTheme,
} from './api.js';

const $ = id => document.getElementById(id);

/* ─── 小工具 ────────────────────────────────────────────────── */

function toast(message, kind = 'ok', timeout = 4200) {
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

/** `expiresAt`（毫秒）→ `YYYY-MM-DD HH:mm`；缺失 / 非法给空串 */
function formatTime(value) {
  const ms = Number(value);
  if (!Number.isFinite(ms) || ms <= 0) return '';
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return '';
  const pad = number => String(number).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} `
    + `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** 把 `/api/session` 的账号数组取出来（两种形状都认） */
function accountsOf(session) {
  const raw = session?.accounts;
  const list = Array.isArray(raw) ? raw : Array.isArray(raw?.accounts) ? raw.accounts : [];
  return list.filter(item => item && PROVIDERS.includes(item.provider));
}

/* ─── 账号列表 ──────────────────────────────────────────────── */

function renderAccounts(accounts) {
  const host = $('accounts');
  $('account-count').textContent = accounts.length ? `${accounts.length} 个` : '';
  if (!accounts.length) {
    host.innerHTML = '<p class="oc-empty">还没有账号。推荐先点上方「连接本机 OpenCode 服务」；也可以添加云端匿名账号。</p>';
    return;
  }
  host.replaceChildren(...accounts.map(account => {
    const row = document.createElement('div');
    row.className = 'oc-row';

    const main = document.createElement('div');
    main.className = 'oc-row-main';
    const name = document.createElement('div');
    name.className = 'oc-row-name';
    name.textContent = account.name || account.nickname || account.id || 'OpenCode 账号';
    main.append(name);

    const meta = document.createElement('div');
    meta.className = 'oc-row-meta';
    const facts = [
      account.provider === LOCAL_PROVIDER ? '本机服务（推荐）' : '云端直连',
      account.plan ? `${account.plan} 通道` : '',
      account.tokenTail ? `Key …${account.tokenTail}` : '',
      account.uid ? `指纹 ${account.uid}` : '',
      formatTime(account.expiresAt) ? `到期 ${formatTime(account.expiresAt)}` : '',
      account.enabled === false ? '已停用' : '',
    ].filter(Boolean);
    meta.textContent = facts.join(' · ') || '—';
    main.append(meta);

    const remove = document.createElement('button');
    remove.className = 'oc-btn oc-btn-danger';
    remove.textContent = '删除';
    remove.addEventListener('click', async () => {
      if (!window.confirm(`删除「${name.textContent}」？账号里的 Key 会一起删除。`)) return;
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

    row.append(main, remove);
    return row;
  }));
}

async function loadAccounts() {
  try {
    renderAccounts(accountsOf(await getSession()));
  } catch (error) {
    $('accounts').innerHTML = '';
    $('account-count').textContent = '';
    toast(`读取账号失败：${error.message || error}`, 'bad');
  }
}

/* ─── 模型清单 ──────────────────────────────────────────────── */

function renderModels(manage) {
  const host = $('models');
  const infos = (Array.isArray(manage?.providers) ? manage.providers : [])
    .filter(item => item && PROVIDERS.includes(item.provider));
  const count = infos.reduce((total, item) => total + Number(item?.count || 0), 0);
  $('model-count').textContent = count ? `${count} 个` : '';

  // 本页只列本包两家（缺失 provider 键时按本包处理）
  const models = (Array.isArray(manage?.models) ? manage.models : [])
    .filter(item => item && (item.provider === undefined || PROVIDERS.includes(item.provider)));

  if (!count || !models.length) {
    host.replaceChildren();
    $('model-hint').textContent = '还没有拉到目录。点「刷新模型清单」拉取；本机服务需要 opencode 桌面端正在运行。';
    return;
  }
  const refreshedAt = formatTime(Math.max(0, ...infos.map(item => Number(item.refreshedAt || 0))));
  const remote = infos.some(item => item.source === 'remote');
  $('model-hint').textContent = `${remote ? '上游目录' : '内置清单'}`
    + (refreshedAt ? ` · 更新于 ${refreshedAt}` : '');

  host.replaceChildren(...models.map(model => {
    const chip = document.createElement('code');
    chip.className = 'oc-chip';
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

/* ─── 添加账号 ──────────────────────────────────────────────── */

/** 添加成功后的统一收尾：清输入、刷账号与目录 */
async function afterAdded(data) {
  toast(`✅ 已添加 ${data?.account?.name || 'OpenCode 账号'}`);
  $('api-key').value = '';
  $('name').value = '';
  // 目录跟着刷新一次：加完账号就能直接选模型，不必再去点刷新
  await Promise.all([loadAccounts(), loadModels()]);
  refreshDirectory(true);
}

/** 连接本机 opencode 服务（推荐）；留空地址/密码 = 自动读 service.json */
async function addLocal() {
  const button = $('add-local');
  busy(button, '连接中…');
  try {
    const data = await addLocalAccount({
      baseUrl: $('local-url').value.trim(),
      password: $('local-password').value.trim(),
      name: $('local-name').value.trim(),
    });
    toast(`✅ 已连接 ${data?.account?.name || '本机 OpenCode 服务'}`);
    $('local-password').value = '';
    await Promise.all([loadAccounts(), loadModels()]);
    refreshDirectory(true);
  } catch (error) {
    toast(String(error.message || error), 'bad');
  } finally {
    busy(button);
  }
}

/** 匿名免费通道（不填 Key；有本机 Key 会自动优先用） */
async function addAnonymous() {
  const button = $('use-local');
  busy(button, '添加中…');
  try {
    // 空 Key 时 Rust 侧强制匿名车道，tier 传了也不会生效，索性不传
    const data = await addAnonymousAccount({
      baseUrl: $('base-url').value.trim(),
      name: $('name').value.trim(),
    });
    await afterAdded(data);
  } catch (error) {
    toast(String(error.message || error), 'bad');
  } finally {
    busy(button);
  }
}

/** 手工粘贴 Key */
async function addWithKey() {
  const button = $('add');
  const key = $('api-key').value.trim();
  if (!key) {
    toast('先粘贴 API Key', 'bad');
    $('api-key').focus();
    return;
  }
  const payload = {
    apiKey: key,
    tier: $('tier').value,
    baseUrl: $('base-url').value.trim(),
    name: $('name').value.trim(),
  };
  busy(button, '校验中…');
  try {
    const data = await addAccount(payload);
    await afterAdded(data);
  } catch (error) {
    toast(String(error.message || error), 'bad');
  } finally {
    busy(button);
  }
}

/** 点「刷新免费模型」或添加成功后调用 */
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

/* ─── 装配 ──────────────────────────────────────────────────── */

requestTheme();
$('add-local').addEventListener('click', addLocal);
$('use-local').addEventListener('click', addAnonymous);
$('add').addEventListener('click', addWithKey);
$('refresh-models').addEventListener('click', () => refreshDirectory(false));
$('reload').addEventListener('click', () => {
  void loadAccounts();
  void loadModels();
});
$('api-key').addEventListener('keydown', event => {
  if (event.key === 'Enter') void addWithKey();
});

void loadAccounts();
void loadModels();
