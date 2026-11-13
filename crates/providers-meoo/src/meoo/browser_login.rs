//! 浏览器 OAuth 登录：本机 loopback 收回调 → 换票 → 组装账号。
//!
//! ── 为什么自己起监听器 ──────────────────────────────────────
//! 官方 CLI 用的是 `http://127.0.0.1:<随机端口>/oauth/callback`（OAuth
//! 公共客户端允许动态端口）。网关自己的 loopback 端口路径是另一套约定，
//! 拼上去未必被授权服务接受。插件进程本来就能绑端口，所以照抄官方 CLI：
//! 自己 listen(0)，把 `redirect_uri` 原样用于授权与换票。
//!
//! ── 与抓 Cookie 派（wpslingxi）的差别 ───────────────────────
//! 不需要 CDP，不需要临时 Chrome profile，也不碰用户日常浏览器的登录态。
//! `LoginStart.url` 给授权地址，`external_browser=true` 让网关用系统浏览器
//! 打开；浏览器授权完成后会跳回本机监听器，插件在后台把 code 换成账号，
//! 网关按 2 秒间隔问 `login.poll` 就能拿到。
//!
//! ── 超时与取消 ──────────────────────────────────────────────
//! 5 分钟超时；`login.cancel` 或重新发起时 abort 后台任务（监听器随任务
//! 一起 drop，不需要手动关端口）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use percent_encoding::percent_decode_str;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

use super::{client, credentials, endpoints, oauth, opener};
use crate::http;

const LOGIN_TIMEOUT_MS: i64 = 5 * 60 * 1_000;
const POLL_INTERVAL_MS: u64 = 2_000;

#[derive(Clone, Debug)]
enum FlowState {
    Waiting { hint: String },
    Captured { account: proto::NormalizedAccount },
    Failed(String),
}

#[derive(Clone, Debug)]
struct Flow {
    created_ms: i64,
    state: FlowState,
    abort: Option<AbortHandle>,
    /// 「重新打开授权页」要复用同一轮 PKCE 的 URL，换了地址 state 就对不上。
    authorize_url: String,
}

fn flows() -> &'static Mutex<HashMap<String, Flow>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Flow>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_flows<R>(f: impl FnOnce(&mut HashMap<String, Flow>) -> R) -> R {
    let mut guard = flows().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

fn set_state(flow: &str, state: FlowState) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow) {
            entry.state = state;
        }
    });
}

fn close_all() {
    let handles = with_flows(|table| {
        let list: Vec<AbortHandle> = table.values().filter_map(|flow| flow.abort.clone()).collect();
        table.clear();
        list
    });
    for handle in handles {
        handle.abort();
    }
}

const INSTRUCTIONS: &str = "已请求系统默认浏览器打开 Meoo 官方授权页，登录并点「授权」。完成后插件自动收下账号；\
     该流程使用官方 OAuth（PKCE），不会读取你浏览器的 Cookie。\
     若没看到浏览器窗口，请点页面上的授权链接或复制地址手动打开。";

/// 插件自有 UI 页只有 `POST /api/accounts` → `account.normalize` 一条通道，
/// 而 `normalize` 只有「成功返回账号 / 失败返回字符串」两个出口。浏览器登录
/// 需要「已开始 / 还在等 / 已取消」三种非终态，所以借错误通道用前缀回传。
///
/// 前缀必须与 `ui/meoo/signal.js` 逐字一致；网关还会给错误文本套一层自己的
/// 前缀，所以前端按 `indexOf` 定位，不能用 `startsWith`。
pub const UI_START: &str = "browser-login-start";
pub const UI_POLL: &str = "browser-login-poll";
pub const UI_CANCEL: &str = "browser-login-cancel";
pub const UI_REOPEN: &str = "browser-login-reopen";

pub const PREFIX_START: &str = "MEOO_LOGIN_START|";
pub const PREFIX_PENDING: &str = "MEOO_LOGIN_PENDING|";
pub const PREFIX_CANCELLED: &str = "MEOO_LOGIN_CANCELLED|";

/// 信令正文用 `|` 分段，提示文本里的分隔符和换行都得抹平。
fn sanitize(text: &str) -> String {
    text.replace('|', " ").replace(['\n', '\r'], " ").trim().to_string()
}

/// START 正文：`<flow>|<url>|<hint>|<1|0>`。末位是「系统浏览器已拉起」，
/// 前端据此决定要不要再尝试 `window.open`。URL 的参数都经过百分号编码，
/// hint 已 sanitize，两者都不会出现裸 `|`。
fn start_signal(flow: &str, url: &str, hint: &str, opened: bool) -> String {
    format!(
        "{PREFIX_START}{flow}|{url}|{}|{}",
        sanitize(hint),
        if opened { '1' } else { '0' }
    )
}

fn pending_signal(hint: &str) -> String {
    format!("{PREFIX_PENDING}{}", sanitize(hint))
}

/// 页面可能带着失效的 flow（网关重启 / 页面缓存旧值）。只要表里还有一轮在
/// 跑就接着用，别让用户对着已经授权成功的浏览器窗口重来一次。
fn resolve_flow(payload: &Value) -> String {
    let given = payload
        .get("flow")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !given.is_empty() && with_flows(|table| table.contains_key(&given)) {
        return given;
    }
    with_flows(|table| {
        table
            .iter()
            .max_by_key(|(_, flow)| flow.created_ms)
            .map(|(id, _)| id.clone())
            .unwrap_or(given)
    })
}

/// 返回 `None` 表示这不是登录动作，交给 `credentials::normalize` 按手填 AK 处理。
///
/// ── 为什么 START 要自己开窗、还要额外带 URL ───────────────────
/// 标准 `login.start` RPC 由网关读 `LoginStart.url` 打开系统浏览器；但页面
/// 走的是 `account.normalize` 错误通道，网关只会看到一条错误文本，不会替我们
/// 开窗，页面自己的 `window.open` 又会被 webview / iframe 拦掉。所以由插件
/// 进程调系统默认浏览器（见 `opener.rs`），授权地址仍随信令回传，供「重新
/// 打开」与手动复制兜底；末段 `1|0` 告诉前端是否已拉起，避免重复开窗。
pub async fn ui_dispatch(
    ctx: &PluginCtx,
    payload: &Value,
) -> Option<Result<proto::NormalizedAccount, String>> {
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match action {
        UI_START => Some(match start(ctx, "browser", payload).await {
            Ok(started) => {
                // 只在这条页面通道里自己开窗；标准 login.start 由网关负责，
                // 两条通道都开会出现两个授权窗口。
                let opened = opener::open(&started.url);
                if let Err(ref reason) = opened {
                    agent2api_provider_sdk::log_warn!(
                        "[Meoo] 自动打开系统浏览器失败：{reason}"
                    );
                }
                let mut hint = sanitize(&started.instructions);
                if opened.is_err() {
                    hint.push_str(
                        " 自动打开浏览器失败，请点页面上的授权链接，或复制授权地址到浏览器打开。",
                    );
                }
                Err(start_signal(
                    &started.flow,
                    &started.url,
                    &hint,
                    opened.is_ok(),
                ))
            }
            Err(error) => Err(error),
        }),
        UI_REOPEN => {
            let flow = resolve_flow(payload);
            let url = with_flows(|table| {
                table
                    .get(&flow)
                    .map(|entry| entry.authorize_url.clone())
                    .unwrap_or_default()
            });
            let hint = if url.is_empty() {
                "这次登录流程已失效，请重新点「打开浏览器登录」".to_string()
            } else {
                match opener::open(&url) {
                    Ok(()) => {
                        "已再次拉起系统默认浏览器，请在浏览器里登录并点「授权」…".to_string()
                    }
                    Err(reason) => format!(
                        "自动打开浏览器失败（{reason}），请复制下面的授权地址到浏览器打开"
                    ),
                }
            };
            // 复用 PENDING 前缀：点「重新打开」不是终态，页面应继续轮询；
            // 提示文本顺带把这次开窗的结果告诉用户。
            Some(Err(pending_signal(&hint)))
        }
        UI_POLL => {
            let flow = resolve_flow(payload);
            Some(match poll(ctx, &flow).await {
                Ok(proto::LoginPoll::Done { account }) => Ok(account),
                Ok(proto::LoginPoll::Pending { hint }) => {
                    Err(pending_signal(&hint.unwrap_or_default()))
                }
                Ok(proto::LoginPoll::Failed { message }) => Err(message),
                Err(error) => Err(error),
            })
        }
        UI_CANCEL => {
            let flow = resolve_flow(payload);
            let _ = cancel(ctx, &flow).await;
            Some(Err(PREFIX_CANCELLED.to_string()))
        }
        _ => None,
    }
}

pub async fn start(
    ctx: &PluginCtx,
    method: &str,
    _payload: &Value,
) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err("Meoo 请点「网页登录」用浏览器授权；或选「填写凭证」粘贴项目服务 AK".to_string());
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("无法在本机启动登录回调监听：{error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("无法读取登录回调端口：{error}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{}", endpoints::OAUTH_CALLBACK_PATH);
    let pkce = oauth::new_pkce()?;
    let api_base = endpoints::api_base().to_string();
    let authorize = oauth::authorize_url(&api_base, &redirect_uri, &pkce);

    close_all();
    let flow = format!("meoo-{}", pkce.state);
    let task_flow = flow.clone();
    let task_ctx = ctx.clone();
    let verifier = pkce.verifier.clone();
    let expected_state = pkce.state.clone();
    let task_redirect = redirect_uri.clone();
    let handle = tokio::spawn(async move {
        let outcome = run_callback(
            listener,
            &task_ctx,
            &expected_state,
            &verifier,
            &task_redirect,
        )
        .await;
        let state = match outcome {
            Ok(account) => FlowState::Captured { account },
            Err(message) => FlowState::Failed(message),
        };
        set_state(&task_flow, state);
    });
    with_flows(|table| {
        table.insert(
            flow.clone(),
            Flow {
                created_ms: http::now_ms(),
                state: FlowState::Waiting {
                    hint: "等待你在浏览器里完成 Meoo 授权…".to_string(),
                },
                abort: Some(handle.abort_handle()),
                authorize_url: authorize.clone(),
            },
        )
    });
    agent2api_provider_sdk::log_info!("[Meoo] 已发起浏览器 OAuth 登录（端口 {port}）");

    Ok(proto::LoginStart {
        flow,
        url: authorize,
        instructions: INSTRUCTIONS.to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        external_browser: true,
        allowed_hosts: vec!["meoo.com".to_string(), "127.0.0.1".to_string()],
    })
}

/// 监听一次回调（最多 5 分钟），把 code 换成账号。
async fn run_callback(
    listener: TcpListener,
    ctx: &PluginCtx,
    expected_state: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<proto::NormalizedAccount, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS as u64);
    let query = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("浏览器授权等待超时，请重新点「网页登录」".to_string());
        }
        let (mut stream, _) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| "浏览器授权等待超时，请重新点「网页登录」".to_string())?
            .map_err(|error| format!("登录回调连接失败：{error}"))?;
        let request = read_request(&mut stream).await?;
        let first_line = request.lines().next().unwrap_or("").to_string();
        let path = first_line.split_whitespace().nth(1).unwrap_or("");
        if !path.starts_with(endpoints::OAUTH_CALLBACK_PATH) {
            let _ = respond(&mut stream, 404, "Not Found").await;
            continue;
        }
        let state = query_value(&first_line, "state").unwrap_or_default();
        if state.is_empty() || state != expected_state {
            let _ = respond(&mut stream, 400, "授权请求校验失败（state 不匹配）").await;
            return Err("授权回调的 state 不匹配，请重新点「网页登录」".to_string());
        }
        if let Some(error) = query_value(&first_line, "error") {
            let detail = query_value(&first_line, "error_description")
                .unwrap_or_else(|| "用户取消了授权".to_string());
            let _ = respond(&mut stream, 400, &format!("未完成授权：{detail}")).await;
            return Err(format!("Meoo 授权未完成：{error} / {detail}"));
        }
        let Some(code) = query_value(&first_line, "code") else {
            let _ = respond(&mut stream, 400, "回调里没有授权码").await;
            return Err("授权回调里没有 code，请重新点「网页登录」".to_string());
        };
        let _ = respond(&mut stream, 200, "授权成功，可以关闭此页面并返回 Agent2API。").await;
        break code;
    };

    let client = http::client(None, &ctx.env.proxy);
    let token = oauth::exchange_code(
        &client,
        endpoints::api_base(),
        &query,
        redirect_uri,
        verifier,
    )
    .await?;
    let identity = client::identity(&client, &token.access_token).await?;
    let project = credentials::pick(&identity, &["project_url_id", "projectUrlId"]);

    // 项目服务 AK 只是「尽量当场拿全」；拿不到也不该让登录失败 —— 请求时
    // `client::resolve_service_ak` 还会按项目列表重试一次。
    let mut service_ak = String::new();
    if !project.is_empty() {
        service_ak = client::cloud_environment(&client, &token.access_token, &project)
            .await
            .ok()
            .map(|value| client_service_ak(&value))
            .unwrap_or_default();
    }
    Ok(credentials::from_login(&token, &identity, &project, &service_ak))
}

fn client_service_ak(environment: &Value) -> String {
    let direct = credentials::pick(environment, &["meooProjectApiKey", "meoo_project_api_key"]);
    if !direct.is_empty() {
        return direct;
    }
    environment
        .get("instance")
        .map(|value| credentials::pick(value, &["meooProjectApiKey"]))
        .unwrap_or_default()
}

async fn read_request(stream: &mut TcpStream) -> Result<String, String> {
    let mut buffer = [0u8; 4096];
    let mut text = String::new();
    for _ in 0..8 {
        let read = stream
            .read(&mut buffer)
            .await
            .map_err(|error| format!("读取登录回调失败：{error}"))?;
        if read == 0 {
            break;
        }
        text.push_str(&String::from_utf8_lossy(&buffer[..read]));
        if text.contains("\r\n\r\n") || text.len() > 8_192 {
            break;
        }
    }
    if text.is_empty() {
        return Err("登录回调是空请求".to_string());
    }
    Ok(text)
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) -> Result<(), String> {
    let reason = if status == 200 { "OK" } else if status == 404 { "Not Found" } else { "Bad Request" };
    let body = format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>Meoo 登录</title><body style=\"font:14px/1.7 system-ui;padding:48px;color:#222\"><h2>{message}</h2></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| format!("回复登录回调失败：{error}"))
}

fn query_value(first_line: &str, key: &str) -> Option<String> {
    let target = first_line.split_whitespace().nth(1)?;
    let (_, query) = target.split_once('?')?;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key {
            return Some(percent_decode_str(value).decode_utf8_lossy().into_owned());
        }
    }
    None
}

pub async fn poll(_ctx: &PluginCtx, flow: &str) -> Result<proto::LoginPoll, String> {
    let entry = with_flows(|table| table.get(flow).cloned());
    let Some(entry) = entry else {
        return Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效（插件重启过？），请重新点「网页登录」".to_string(),
        });
    };
    if http::now_ms() - entry.created_ms > LOGIN_TIMEOUT_MS {
        if let Some(handle) = entry.abort {
            handle.abort();
        }
        with_flows(|table| {
            table.remove(flow);
        });
        return Ok(proto::LoginPoll::Failed {
            message: "浏览器授权等待超时，请重新点「网页登录」".to_string(),
        });
    }
    match entry.state {
        FlowState::Waiting { hint } => Ok(proto::LoginPoll::Pending { hint: Some(hint) }),
        FlowState::Captured { account } => {
            with_flows(|table| {
                table.remove(flow);
            });
            Ok(proto::LoginPoll::Done { account })
        }
        FlowState::Failed(message) => {
            with_flows(|table| {
                table.remove(flow);
            });
            Ok(proto::LoginPoll::Failed { message })
        }
    }
}

pub async fn cancel(_ctx: &PluginCtx, flow: &str) -> Result<(), String> {
    let entry = with_flows(|table| table.remove(flow));
    if let Some(entry) = entry {
        if let Some(handle) = entry.abort {
            handle.abort();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_callback_query() {
        let line = "GET /oauth/callback?state=a%20b&code=x%2Fy&error_description=no%20thanks HTTP/1.1";
        assert_eq!(query_value(line, "state"), Some("a b".to_string()));
        assert_eq!(query_value(line, "code"), Some("x/y".to_string()));
        assert_eq!(query_value(line, "error"), None);
    }

    #[test]
    fn ui_signal_text_is_pipe_safe_and_prefixes_are_distinct() {
        assert_eq!(sanitize("  a|b\nc\r  "), "a b c");
        assert!(PREFIX_START.ends_with('|'));
        assert!(PREFIX_PENDING.ends_with('|'));
        assert!(PREFIX_CANCELLED.ends_with('|'));
        assert_ne!(PREFIX_START, PREFIX_PENDING);
        assert_ne!(PREFIX_PENDING, PREFIX_CANCELLED);
    }

    #[test]
    fn start_signal_puts_open_flag_last_and_keeps_url() {
        let url = "https://meoo.com/oauth/authorize?client_id=a&state=b%2Fc&redirect_uri=http%3A%2F%2F127.0.0.1%3A5182%2Foauth%2Fcallback";
        let signal = start_signal("meoo-x", url, "请|在 浏览器\n授权", true);
        let payload = signal.strip_prefix(PREFIX_START).unwrap_or("");
        let parts: Vec<&str> = payload.split('|').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "meoo-x");
        assert_eq!(parts[1], url);
        assert_eq!(parts[2], "请 在 浏览器 授权");
        assert_eq!(parts[3], "1");
        assert!(start_signal("meoo-x", url, "", false).ends_with("|0"));
    }

    #[test]
    fn pending_signal_sanitizes_hint() {
        assert_eq!(pending_signal(" 等|待\n授权 "), format!("{PREFIX_PENDING}等 待 授权"));
    }

    #[test]
    fn ui_actions_are_distinct() {
        assert_ne!(UI_START, UI_POLL);
        assert_ne!(UI_START, UI_CANCEL);
        assert_ne!(UI_START, UI_REOPEN);
        assert_ne!(UI_POLL, UI_REOPEN);
        assert_ne!(UI_CANCEL, UI_REOPEN);
    }

    #[test]
    fn resolve_flow_keeps_given_when_no_flow_is_running() {
        close_all();
        let payload = serde_json::json!({ "action": UI_POLL, "flow": "meoo-x" });
        assert_eq!(resolve_flow(&payload), "meoo-x");
    }

    #[test]
    fn service_ak_reads_instance() {
        assert_eq!(
            client_service_ak(&serde_json::json!({"instance": {"meooProjectApiKey": "ak"}})),
            "ak"
        );
    }
}
