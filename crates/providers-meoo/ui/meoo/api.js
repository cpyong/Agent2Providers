/**
 * Meoo 页面 ↔ 网关管理 API 的最小客户端。
 *
 * ── 凭证从哪来 ──────────────────────────────────────────────
 * 与其余独立包同一套口径：
 *   · web 面板（页面由网关托管）：同源，HttpOnly cookie 直接生效；
 *   · 桌面端 / 手机：iframe 与网关不同源，宿主在页面加载后用
 *     `postMessage` 递一份 `{ headers }` 进来。
 * 页面自己不猜「我在哪」：先等一小段时间的消息，拿不到就按同源继续。
 *
 * ── 页面不直连 Meoo ─────────────────────────────────────────
 * 控制面要 OAuth access token、模型网关要项目服务 AK，两者都不能暴露给
 * 浏览器页面；上游也不给 CORS。所有出网都由插件进程代打，页面只调 `/api/*`。
 */

/** 宿主递进来的凭证头（空对象 = 同源形态，走 cookie） */
let panelHeaders = {};
let headersReady = false;

/** 本插件对应的 provider id（与 pack.json / Rust 侧 `meoo::ID` 一致） */
export const PROVIDER = 'meoo';

function applyTheme(mode) {
  const value = mode === 'dark' || mode === 'light' ? mode : 'system';
  document.documentElement.dataset.theme = value;
  const systemDark = window.matchMedia?.('(prefers-color-scheme: dark)')?.matches === true;
  document.documentElement.style.colorScheme =
    value === 'dark' || (value === 'system' && systemDark) ? 'dark' : 'light';
}

window.addEventListener('message', event => {
  const data = event.data;
  if (!data || typeof data !== 'object') return;
  if (data.type === 'agent2api:panel-theme') applyTheme(data.theme);
});

const CREDENTIAL_TIMEOUT_MS = 3000;

function waitForCredentials() {
  if (headersReady) return Promise.resolve();
  return new Promise(resolve => {
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      headersReady = true;
      resolve();
    };
    const onMessage = event => {
      const data = event.data;
      if (!data || data.type !== 'agent2api:panel-credentials') return;
      const headers = data.headers;
      if (headers && typeof headers === 'object') {
        for (const [name, value] of Object.entries(headers)) {
          if (typeof value === 'string' && value) panelHeaders[name] = value;
        }
      }
      finish();
    };
    window.addEventListener('message', onMessage);
    try {
      window.parent?.postMessage({ type: 'agent2api:request-credentials' }, '*');
    } catch {
      /* 父窗口不可达（页面被单独打开）：按同源处理 */
    }
    window.setTimeout(finish, CREDENTIAL_TIMEOUT_MS);
  });
}

export function requestTheme() {
  try {
    window.parent?.postMessage({ type: 'agent2api:request-theme' }, '*');
  } catch {
    /* 单独打开：保持 system */
  }
}

/** 把三种响应信封归一成 `{ data, error }`；`error` 非空即失败 */
function unwrap(payload) {
  if (!payload || typeof payload !== 'object') return { data: payload, error: '' };
  if (Object.prototype.hasOwnProperty.call(payload, 'success')) {
    if (payload.success === false) {
      return { data: null, error: String(payload.error || '请求失败') };
    }
    return { data: payload.data, error: '' };
  }
  if (payload.error) {
    const error = payload.error;
    const message = typeof error === 'string'
      ? error
      : String(error.message || error.type || '请求失败');
    return { data: null, error: message };
  }
  return { data: payload, error: '' };
}

export async function request(path, options = {}) {
  await waitForCredentials();
  const method = options.method || 'GET';
  const headers = { Accept: 'application/json', ...panelHeaders };
  const init = { method, headers, credentials: 'include' };
  if (options.body !== undefined || method !== 'GET') {
    headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(options.body ?? {});
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch (error) {
    throw new Error(`无法连接网关：${error instanceof Error ? error.message : String(error)}`);
  }
  const text = await response.text();
  let payload = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      throw new Error(text.trim() || `网关返回了非 JSON 响应（HTTP ${response.status}）`);
    }
  }
  const { data, error } = unwrap(payload);
  if (error) throw new Error(error);
  if (!response.ok) throw new Error(`请求失败（HTTP ${response.status}）`);
  return data;
}

/* ─── 具体端点 ──────────────────────────────────────────────── */

const accountPath = id => `/api/accounts/${encodeURIComponent(String(id || ''))}`;

/**
 * 手动添加账号：把项目服务 AK / 凭证 JSON 交给插件的 `account.normalize`。
 * `payload` 字段由插件侧 `unwrap_payload` 解包（网关通用表单也可能直接给裸对象）。
 */
export const addAccount = payload =>
  request('/api/accounts', { method: 'POST', body: { provider: PROVIDER, ...payload } });

/**
 * 浏览器登录：借 `/api/accounts` 的 `action` 分流到插件里的同一套 OAuth flow。
 * 三个动作并不是「添加账号」本身：`start` / `cancel` 用带外信令回传状态，
 * `poll` 只有真正拿到账号时才返回成功、让网关落库。前缀由 signal.js 识别，
 * 前缀以外的错误按真实失败展示。
 */
export const browserLoginStart = () =>
  request('/api/accounts', {
    method: 'POST',
    body: { provider: PROVIDER, action: 'browser-login-start' },
  });

export const browserLoginPoll = flow =>
  request('/api/accounts', {
    method: 'POST',
    body: { provider: PROVIDER, action: 'browser-login-poll', flow },
  });

export const browserLoginCancel = flow =>
  request('/api/accounts', {
    method: 'POST',
    body: { provider: PROVIDER, action: 'browser-login-cancel', flow },
  });

/**
 * 「浏览器没自动打开」的兜底按钮：让插件进程再拉一次系统默认浏览器。
 * 复用同一轮 PKCE 的授权 URL，不会新开 flow。响应借 PENDING 信令带回提示，
 * 调用方按错误文本解析即可。
 */
export const browserLoginReopen = flow =>
  request('/api/accounts', {
    method: 'POST',
    body: { provider: PROVIDER, action: 'browser-login-reopen', flow },
  });

export const deleteAccount = id => request(accountPath(id), { method: 'DELETE' });

/** 首屏快照（账号列表在这里） */
export const getSession = () => request('/api/session');

/** 模型管理视图 */
export const getModels = () => request('/api/models/manage');

/** 只刷本家目录 */
export const refreshModels = () =>
  request('/api/models/refresh', { method: 'POST', body: { providers: [PROVIDER] } });

/** 单个账号执行每日登录赠积分（与签到中心同一条接口） */
export const runCheckin = id => request('/api/accounts/checkin', {
  method: 'POST',
  body: { id },
});
