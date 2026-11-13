/**
 * Today AI 账号页的交互。
 *
 * 三块：添加账号（手机号验证码 / 粘贴令牌）、账号列表、模型清单。
 * 端点与凭证都在 api.js 里，这一层只管 DOM 与状态。
 */

import {
  addAccount,
  deleteAccount,
  getModels,
  getSession,
  OTP_SENT,
  PROVIDER,
  refreshModels,
  requestTheme,
} from './api.js';

const $ = id => document.getElementById(id);
const CODE_SECONDS = 60;

/** 发码冷却的截止时刻（毫秒）；0 = 可以发 */
let sendLockedUntil = 0;
let sendTimer = 0;

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
  return list.filter(item => item && item.provider === PROVIDER);
}

/**
 * 发码那一步的回报判据。
 *
 * 网关只有「产出账号」与「报错」两个出口，而发码两者都不是 —— 插件用
 * `OTP_SENT` 前缀借错误通道回报（见 Rust 侧 `credentials::normalize`）。
 *
 * 注意**不能**用 `startsWith`：网关会在前面套一层自己的文案
 * （`提供商插件调用失败（account.normalize）：OTP_SENT …`，见
 * `api/accounts.rs` 的 `add_plugin_account`），前缀匹配会永远落空，
 * 用户看到的就是「发码失败」—— 明明短信已经发出去了。这里按标记定位，
 * 再把标记之后的正文当成功提示。
 */
function otpSentMessage(error) {
  const text = String(error?.message || error || '');
  const at = text.indexOf(OTP_SENT);
  if (at < 0) return '';
  const rest = text.slice(at + OTP_SENT.length).replace(/^[：:\s]+/, '').trim();
  return rest || '验证码已发送，请查收短信';
}

/* ─── 账号列表 ──────────────────────────────────────────────── */

function renderAccounts(accounts) {
  const host = $('accounts');
  $('account-count').textContent = accounts.length ? `${accounts.length} 个` : '';
  if (!accounts.length) {
    host.innerHTML = '<p class="ta-empty">还没有账号。填写手机号并获取验证码即可添加。</p>';
    return;
  }
  host.replaceChildren(...accounts.map(account => {
    const row = document.createElement('div');
    row.className = 'ta-row';

    const main = document.createElement('div');
    main.className = 'ta-row-main';
    const name = document.createElement('div');
    name.className = 'ta-row-name';
    name.textContent = account.name || account.nickname || account.id || 'Today AI 账号';
    main.append(name);

    const meta = document.createElement('div');
    meta.className = 'ta-row-meta';
    const facts = [
      account.uid ? `uid ${account.uid}` : '',
      account.phone || '',
      formatTime(account.expiresAt) ? `重登期限 ${formatTime(account.expiresAt)}` : '',
      account.enabled === false ? '已停用' : '',
    ].filter(Boolean);
    meta.textContent = facts.join(' · ') || '—';
    main.append(meta);

    const remove = document.createElement('button');
    remove.className = 'ta-btn ta-btn-danger';
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
  const info = (Array.isArray(manage?.providers) ? manage.providers : [])
    .find(item => item && item.provider === PROVIDER);
  const count = Number(info?.count || 0);
  $('model-count').textContent = count ? `${count} 个` : '';

  // 目录条目在多家之间共用 id，本页只列本家那几条（缺失 provider 键时按本家处理）
  const models = (Array.isArray(manage?.models) ? manage.models : [])
    .filter(item => item && (item.provider === undefined || item.provider === PROVIDER));

  if (!count || !models.length) {
    host.replaceChildren();
    $('model-hint').textContent = '还没有拉到目录。先添加账号，再点「刷新模型清单」。';
    return;
  }
  const refreshedAt = formatTime(info.refreshedAt);
  $('model-hint').textContent = `${info.source === 'remote' ? '上游目录' : '内置清单'}`
    + (refreshedAt ? ` · 更新于 ${refreshedAt}` : '');

  host.replaceChildren(...models.map(model => {
    const chip = document.createElement('code');
    chip.className = 'ta-chip';
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

/** 冷却倒计时（按钮上的秒数就是「多久后能再发」） */
function startCountdown(button) {
  sendLockedUntil = Date.now() + CODE_SECONDS * 1000;
  window.clearInterval(sendTimer);
  const tick = () => {
    const left = Math.ceil((sendLockedUntil - Date.now()) / 1000);
    if (left <= 0) {
      window.clearInterval(sendTimer);
      sendLockedUntil = 0;
      busy(button);
      return;
    }
    button.disabled = true;
    button.textContent = `${left} 秒后重发`;
  };
  tick();
  sendTimer = window.setInterval(tick, 1000);
}

function phoneValue() {
  return $('phone').value.trim();
}

async function sendCode() {
  const button = $('send');
  if (sendLockedUntil > Date.now()) return;
  const phone = phoneValue();
  if (!phone) {
    toast('先填手机号', 'bad');
    $('phone').focus();
    return;
  }
  busy(button, '发送中…');
  try {
    await addAccount({ action: 'sms-send', phone });
    // 走到了成功分支说明网关以后可能给发码一条正经的成功出口，
    // 这时同样按「已发送」处理。
    toast('验证码已发送，请查收短信');
    startCountdown(button);
  } catch (error) {
    const sent = otpSentMessage(error);
    if (sent) {
      toast(sent);
      startCountdown(button);
      return;
    }
    busy(button);
    toast(String(error.message || error), 'bad');
  }
}

async function addWithCode() {
  const button = $('add');
  const phone = phoneValue();
  const code = $('code').value.trim();
  if (!phone) {
    toast('先填手机号', 'bad');
    $('phone').focus();
    return;
  }
  if (!/^\d{6}$/.test(code)) {
    toast('验证码是 6 位数字', 'bad');
    $('code').focus();
    return;
  }
  busy(button, '登录中…');
  try {
    const data = await addAccount({
      action: 'sms-login',
      phone,
      code,
      name: $('name').value.trim(),
    });
    const label = data?.account?.name || '账号';
    toast(`✅ 已添加 ${label}`);
    $('code').value = '';
    $('name').value = '';
    // 目录跟着刷新一次：用户加完账号就能直接选模型，不必再去模型页点按钮
    await Promise.all([loadAccounts(), loadModels()]);
    refreshDirectory(true);
  } catch (error) {
    toast(String(error.message || error), 'bad');
  } finally {
    busy(button);
  }
}

async function addWithToken() {
  const button = $('add-token');
  const token = $('token').value.trim();
  if (!token) {
    toast('先粘贴令牌', 'bad');
    $('token').focus();
    return;
  }
  busy(button, '添加中…');
  try {
    const data = await addAccount({ session: token });
    toast(`✅ 已添加 ${data?.account?.name || '账号'}`);
    $('token').value = '';
    await Promise.all([loadAccounts(), loadModels()]);
    refreshDirectory(true);
  } catch (error) {
    toast(String(error.message || error), 'bad');
  } finally {
    busy(button);
  }
}

/** 点「刷新模型清单」或添加成功后调用 */
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
$('send').addEventListener('click', sendCode);
$('add').addEventListener('click', addWithCode);
$('add-token').addEventListener('click', addWithToken);
$('refresh-models').addEventListener('click', () => refreshDirectory(false));
$('reload').addEventListener('click', () => {
  void loadAccounts();
  void loadModels();
});
$('code').addEventListener('keydown', event => {
  if (event.key === 'Enter') void addWithCode();
});
$('phone').addEventListener('keydown', event => {
  if (event.key === 'Enter') void sendCode();
});

void loadAccounts();
void loadModels();
