/**
 * 带外信令解析（纯函数，无 DOM 依赖，可被 node 直接单测）。
 *
 * ── 为什么需要它 ────────────────────────────────────────────
 * 插件自有页面只有一条通道能触达插件进程：`POST /api/accounts` →
 * `account.normalize`。而 `normalize` 只有「成功返回账号」与「失败返回字符串」
 * 两个出口，浏览器登录却需要「已开始 / 还在等 / 已取消」三种非终态。
 * 于是插件用 `MEOO_LOGIN_*` 前缀借错误通道回传状态（见 Rust 侧
 * `browser_login.rs` 的 `ui_dispatch`）。
 *
 * ── 关键坑：网关会给错误文本加前缀 ──────────────────────────
 * 网关在 `api/accounts.rs` 里会把插件返回的错误包成
 * `提供商插件调用失败（account.normalize）：<原文>`。所以**绝不能**用
 * `startsWith` 判断 —— 那样永远匹配不上，表现就是：浏览器已经打开、用户也
 * 授权成功了，页面却弹一句「登录失败」，之后一次轮询都不发。
 */

/** `start` 回传 flow id + 授权 URL + 提示；后面用 `|` 分段 */
export const LOGIN_START = 'MEOO_LOGIN_START|';
/** `poll` 的等待进度；正文可能为空 */
export const LOGIN_PENDING = 'MEOO_LOGIN_PENDING|';
/** `cancel` 的回执；无正文 */
export const LOGIN_CANCELLED = 'MEOO_LOGIN_CANCELLED|';

/**
 * 从错误文本中取出标记之后的正文。
 *
 * @param {unknown} text 错误文本（网关可能已套上自己的前缀）
 * @param {string} marker 形如 `MEOO_LOGIN_START|` 的标记
 * @returns {string|null} 标记之后的内容；**找不到返回 null**（区别于空串），
 *   空串表示「有信令但没有正文」，例如等待中还没有进度可报。
 */
export function signalOf(text, marker) {
  const whole = String(text == null ? '' : text);
  const at = whole.indexOf(marker);
  if (at < 0) return null;
  return whole.slice(at + marker.length).replace(/^[：:\s]+/, '');
}

/**
 * START 正文：`<flow>|<authorizeUrl>|<instructions>|<1|0>`。
 *
 * 页面走 `account.normalize` 错误通道，网关不会替我们开窗，页面自己的
 * `window.open` 又常被 webview / iframe 拦掉，所以由插件进程调系统默认
 * 浏览器。URL 仍随信令回来，供「重新打开」与手动复制；末位 `1` 表示插件
 * 已成功拉起浏览器，页面就不必再开第二个窗口。授权 URL 的 query 都经过
 * 百分号编码，不会出现裸 `|`。
 *
 * 末位标记是后加的：`hasFlag` 判定让本页仍能解析旧版插件的四段正文。
 */
export function parseStart(payload) {
  const parts = String(payload || '').split('|');
  const last = (parts[parts.length - 1] || '').trim();
  const hasFlag = parts.length >= 4 && (last === '1' || last === '0');
  return {
    flow: (parts[0] || '').trim(),
    url: (parts[1] || '').trim(),
    hint: (hasFlag ? parts.slice(2, -1) : parts.slice(2)).join(' | ').trim(),
    opened: hasFlag ? last === '1' : false,
  };
}
