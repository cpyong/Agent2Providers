/**
 * 带外信令解析（纯函数，无 DOM 依赖，可被 node 直接单测）。
 *
 * ── 为什么需要它 ────────────────────────────────────────────
 * 插件自有页面只有一条通道能触达插件进程：`POST /api/accounts` →
 * `account.normalize`。而 `normalize` 只有「成功返回账号」与「失败返回字符串」
 * 两个出口，浏览器登录却需要「已开始 / 还在等 / 已取消」三种非终态。
 * 于是插件用 `WPSLX_LOGIN_*` 前缀借错误通道回传状态（见 Rust 侧
 * `web_login.rs` 的 `ui_dispatch`）。
 *
 * ── 关键坑：网关会给错误文本加前缀 ──────────────────────────
 * 网关在 `api/accounts.rs` 的 `add_plugin_account` 里会把插件返回的错误
 * 包成 `提供商插件调用失败（account.normalize）：<原文>`。
 * 所以**绝不能**用 `startsWith` 判断 —— 那样永远匹配不上，表现就是：
 * 浏览器窗口已经打开、用户也登录成功了，页面却弹一句「登录失败」，
 * 之后一次轮询都不发，账号自然落不了库。
 *
 * 正确做法是按标记定位，再取标记之后的正文。
 */

/** `start` 回传 flow id；后面用 `|` 拼接给用户看的提示 */
export const LOGIN_START = 'WPSLX_LOGIN_START|';
/** `poll` 的等待进度；正文可能为空 */
export const LOGIN_PENDING = 'WPSLX_LOGIN_PENDING|';
/** `cancel` 的回执；无正文 */
export const LOGIN_CANCELLED = 'WPSLX_LOGIN_CANCELLED|';

/**
 * 从错误文本中取出标记之后的正文。
 *
 * @param {unknown} text 错误文本（网关可能已套上自己的前缀）
 * @param {string} marker 形如 `WPSLX_LOGIN_START|` 的标记
 * @returns {string|null} 标记之后的内容；**找不到返回 null**（区别于空串），
 *   空串表示「有信令但没有正文」，例如等待中还没有进度可报。
 */
export function signalOf(text, marker) {
  const whole = String(text == null ? '' : text);
  const at = whole.indexOf(marker);
  if (at < 0) return null;
  return whole.slice(at + marker.length).replace(/^[：:\s]+/, '');
}
