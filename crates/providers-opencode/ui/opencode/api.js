/**
 * OpenCode 账号页 ↔ 网关管理 API 的最小客户端。
 *
 * ── 凭证从哪来 ──────────────────────────────────────────────
 * 与合集包 `ui/checkin/api.js` 同一套口径：
 *   · **web 面板**（页面由网关托管）：同源，HttpOnly cookie 直接生效；
 *   · **桌面端 / 手机**：iframe 与网关不同源，宿主在页面加载后用
 *     `postMessage` 递一份 `{ headers }` 进来。
 * 页面自己不去猜「我在哪」：先等一小段时间的消息，拿不到就按同源继续，
 * 迟到的消息仍会被后续请求用上。
 *
 * ── 响应信封有三种，都要认 ──────────────────────────────────
 *   · `{ success: true, data: … }`      多数管理接口
 *   · `{ success: false, error: "…" }`  插件 / 账号那族
 *   · `{ error: { message, type } }`    OpenAI 风格（转发 / 计费）
 * 只认一种的后果是错误提示变成 `[object Object]`。
 */

/** 宿主递进来的凭证头（空对象 = 同源形态，走 cookie） */
let panelHeaders = {};
/** 凭证是否已经就位（同源形态永远「就位」） */
let headersReady = false;

/** 云端直连 provider id（与 `pack.json` / Rust 侧 `opencode::ID` 一致） */
export const PROVIDER = 'opencode';
/** 本机服务 provider id（推荐；Rust 侧 `local::ID`） */
export const LOCAL_PROVIDER = 'opencode-local';
/** 本页同时管理这两家账号 */
export const PROVIDERS = [LOCAL_PROVIDER, PROVIDER];

/** 应用宿主告知的主题模式（`light` / `dark` / `system`） */
function applyTheme(mode) {
  const value = mode === 'dark' || mode === 'light' ? mode : 'system';
  document.documentElement.dataset.theme = value;
  const systemDark = window.matchMedia?.('(prefers-color-scheme: dark)')?.matches === true;
  document.documentElement.style.colorScheme =
    value === 'dark' || (value === 'system' && systemDark) ? 'dark' : 'light';
}

/** 主题随时可能被用户改，监听常驻不摘 */
window.addEventListener('message', event => {
  const data = event.data;
  if (!data || typeof data !== 'object') return;
  if (data.type === 'agent2api:panel-theme') applyTheme(data.theme);
});

/** 等宿主递凭证的上限（超时按同源处理，不阻塞请求） */
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
      // 主动要一次：宿主收到后直接回给这个 frame（比干等 load 钩子可靠）
      window.parent?.postMessage({ type: 'agent2api:request-credentials' }, '*');
    } catch {
      /* 父窗口不可达（页面被单独打开）：按同源处理 */
    }
    window.setTimeout(finish, CREDENTIAL_TIMEOUT_MS);
  });
}

/** 主动问一次主题（页面首帧就用上正确的深浅） */
export function requestTheme() {
  try {
    window.parent?.postMessage({ type: 'agent2api:request-theme' }, '*');
  } catch {
    /* 单独打开：保持 system */
  }
}

/** 把三种信封归一成 `{ data, error }`；`error` 非空即失败 */
function unwrap(payload) {
  if (!payload || typeof payload !== 'object') {
    return { data: payload, error: '' };
  }
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

/**
 * 发一条管理 API 请求。
 *
 * `path` 是网关侧的绝对路径（`/api/...`），**不要**拼基址：iframe 的文档本身
 * 就在网关 origin 上，相对路径天然正确。
 */
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

/* ─── 具体端点（页面只调这一层）─────────────────────────────── */

const accountPath = id => `/api/accounts/${encodeURIComponent(String(id || ''))}`;

/**
 * 添加账号。
 *
 * 云端家 payload：`{ apiKey?, tier: 'zen' | 'go', baseUrl?, name? }`；
 * 本机家 payload：`{ baseUrl?, password?, username?, name? }`。
 * 由各自的 `account.normalize` 解析并探活。
 */
export const addAccount = (payload, provider = PROVIDER) =>
  request('/api/accounts', { method: 'POST', body: { provider, ...payload } });

/**
 * 不填 Key 添加云端账号：插件先试环境变量 / 本机 auth.json，
 * 都拿不到就走匿名免费车道。
 *
 * 注意：匿名车道强制 `tier = anon`，前端的「Key 通道」下拉对它无效，
 * 所以这里**不传 tier**（旧版传的 `anonymous: true` Rust 侧从不读取）。
 */
export const addAnonymousAccount = (payload = {}) => addAccount(payload);

/** 添加本机 opencode 服务连接（留空即自动发现 service.json） */
export const addLocalAccount = payload => addAccount(payload, LOCAL_PROVIDER);

export const deleteAccount = id => request(accountPath(id), { method: 'DELETE' });

/** 首屏快照（账号列表在这里，形状见网关 `api::session`） */
export const getSession = () => request('/api/session');

/** 模型管理视图：`{models, providers, mappings, reasoningLevels}` */
export const getModels = () => request('/api/models/manage');

/** 只刷本包两家的目录（body 的可选范围白名单，见网关 `api/models.rs`） */
export const refreshModels = () =>
  request('/api/models/refresh', { method: 'POST', body: { providers: PROVIDERS } });
