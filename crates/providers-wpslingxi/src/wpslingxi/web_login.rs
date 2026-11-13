//! WPS 灵犀的「浏览器登录」：拉起独立浏览器 → 官方登录页 → 读 `wps_sid` → 收账号。
//!
//! ── 为什么不是 Trae 那种「授权页 + loopback 回调 + 换票」 ──────
//! 上游登录走 WPS 账号 SSO，**没有公开的设备码 / OAuth / 换票协议**
//! （`RobbsLuo/Coding2API` 的 `lingxi/credential.py` 与 WPS 社区教程都只教
//! 从 DevTools 复制 Cookie）。没有回调票据可换，虚构一条授权链路只会让
//! 用户对着一个永远完不成的窗口。
//!
//! 于是换个落点：既然凭证本来就在浏览器 cookie jar 里，就让插件**自己**
//! 拉起一个干净的浏览器实例，用户登录完直接从它的调试接口读出来：
//!
//! ```text
//!   login.start  绑定临时 profile → 启动 Chrome/Edge（--remote-debugging-port=0）
//!                → 浏览器写 DevToolsActivePort → 起后台观察任务
//!   watch        每 1.5s 读 CDP Storage.getCookies → 找 wps_sid
//!                → 用 /api/public/v1/account/profile 验明不是游客态
//!                → 写 Captured（只存 Cookie 串，不落账号）
//!   login.poll   读到 Captured → credentials::normalize 生成可落库账号
//!                → 关浏览器 / 删临时 profile / 清 flow
//!   login.cancel 同上，但不产出账号
//! ```
//!
//! ── 为什么用独立临时 profile ────────────────────────────────
//! 不碰用户日常浏览器的登录态与 Cookie 库（那需要解 DPAPI、还可能撞上
//! 运行中的浏览器锁库）；也不要求用户给日常浏览器加调试参数。用完即删。
//!
//! ── 两条入口共用一张 flow 表 ────────────────────────────────
//!   1. 网关通用「网页登录」按钮 → `login.start` / `login.poll` / `login.cancel`
//!   2. 插件自有 UI 页 → `account.normalize` 的 `action` 分流（见 [`ui_dispatch`]）
//!      之所以多这一条：插件页只调网关 `/api/accounts`，拿不到登录 RPC 的
//!      直连路径；`action` 分流是仓库里已有的同款做法（见 Today AI 的短信登录）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::{client, credentials, ID};
use crate::http;

/// flow id 前缀（后面接毫秒时间戳，便于日志里认出是哪一轮）。
const FLOW_PREFIX: &str = "wpslingxi-web-";
/// 网关来问的间隔。
const POLL_INTERVAL_MS: u64 = 2_000;
/// 后台观察浏览器 cookie 的间隔。
const WATCH_INTERVAL: Duration = Duration::from_millis(1_500);
/// 一轮登录的上限（与网关侧 5 分钟超时同量级）。
const LOGIN_TIMEOUT_MS: i64 = 300_000;
/// CDP 的 loopback 请求超时。浏览器还没起来时会连不上，必须快速失败重试。
const CDP_TIMEOUT: Duration = Duration::from_secs(3);
/// 默认打开官方网页版登录页；kdocs 集群可用环境变量改。
const DEFAULT_LOGIN_URL: &str = "https://lingxi.wps.cn/";

/// 给用户看的操作说明（`LoginStart` 与插件页共用一份文案）。
const INSTRUCTIONS: &str = "已自动打开一个独立的浏览器窗口，请用微信扫码或手机号登录 WPS 账号；\
登录完成后会自动收下账号，不用复制 Cookie。若没有看到窗口，请确认本机装有 Chrome / Edge，\
或改用「手动粘贴 Cookie」。";

/// 插件自有 UI 页通过 `POST /api/accounts` 的 `action` 分流用的三个动作。
pub const UI_START: &str = "browser-login-start";
pub const UI_POLL: &str = "browser-login-poll";
pub const UI_CANCEL: &str = "browser-login-cancel";

/// 这三个前缀不是「错误」，是 UI 与插件之间约定好的带外信令：
/// `account.normalize` 只有「成功返回账号」与「失败返回字符串」两种出口，
/// 而登录需要「已开始 / 还在等 / 已取消」三种非终态。用前缀把它们和真正
/// 的失败区分开，UI 只认前缀，其余一律当错误展示。
pub const PREFIX_START: &str = "WPSLX_LOGIN_START|";
pub const PREFIX_PENDING: &str = "WPSLX_LOGIN_PENDING|";
pub const PREFIX_CANCELLED: &str = "WPSLX_LOGIN_CANCELLED|";

// ── 流程表 ────────────────────────────────────────────────────

#[derive(Clone)]
enum FlowState {
    Waiting { hint: String },
    /// 已读到经过资料接口验证的 Cookie；账号由 `poll` 现场生成。
    /// `name` 是资料接口给的昵称（浏览器登录已经出过一次网，别让
    /// `normalize` 因为 uid 已知就跳过昵称、把账号显示成一串 uid）。
    Captured {
        cookie: String,
        name: Option<String>,
    },
    Failed(String),
}

struct Flow {
    /// `None` = 复用外部 CDP 端点（`WPSLINGXI_CDP_PORT`），不负责它的生死。
    child: Option<Child>,
    profile_dir: PathBuf,
    started_ms: i64,
    state: FlowState,
}

fn flows() -> &'static Mutex<HashMap<String, Flow>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Flow>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_flows<T>(action: impl FnOnce(&mut HashMap<String, Flow>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

fn set_state(flow: &str, state: FlowState) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow) {
            entry.state = state;
        }
    });
}

fn alive(flow: &str) -> bool {
    with_flows(|table| table.contains_key(flow))
}

/// 收掉一条：先杀浏览器再删临时 profile，顺序反了会在 Windows 上留一地锁文件。
fn cleanup(entry: &mut Flow) {
    if let Some(child) = entry.child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(&entry.profile_dir);
}

/// 新登录发起前把上一轮收干净：两个浏览器实例同时开着，用户不知道该在
/// 哪个窗口登录，而两个实例的 cookie jar 互不相通。
fn close_all() {
    let mut drained: Vec<Flow> = with_flows(|table| table.drain().map(|(_, flow)| flow).collect());
    for entry in &mut drained {
        cleanup(entry);
    }
}

fn take(flow: &str) -> Option<Flow> {
    with_flows(|table| table.remove(flow))
}

/// 正在进行、且没过期的一轮登录（最新的一条）。
///
/// 为什么需要复用而不是「新的一律重开」：插件页拿到的 flow id 只存在于
/// `normalize` 的错误文本里，网关会给那段文本套一层前缀；万一页面那一侧
/// 解析失手（或用户手快点了两下），再点一次「打开浏览器登录」不该把用户
/// 正在登录的那个窗口杀掉 —— 那会让「登录成功但账号没落库」变得更难解释。
fn reusable() -> Option<String> {
    let now = http::now_ms();
    with_flows(|table| {
        table
            .iter()
            .filter(|(_, flow)| now - flow.started_ms <= LOGIN_TIMEOUT_MS)
            .filter(|(_, flow)| !matches!(flow.state, FlowState::Failed(_)))
            .max_by_key(|(_, flow)| flow.started_ms)
            .map(|(id, _)| id.clone())
    })
}

/// 进程内最新的一条 flow，不校验状态。
///
/// 插件页的 poll 可能带着一个「插件已经不认识」的 id（流程被网关重启过、
/// 或页面缓存了旧值）。只要当前只有一轮登录在跑，就按它继续 —— 这比让用户
/// 对着一个已经登录完成的浏览器窗口重来一次要好。
fn newest_flow() -> Option<String> {
    with_flows(|table| {
        table
            .iter()
            .max_by_key(|(_, flow)| flow.started_ms)
            .map(|(id, _)| id.clone())
    })
}

/// 超时后主动收掉浏览器。用户关掉登录窗口时网关可能不再来 poll，
/// 也不一定调 cancel —— 不能让那个临时浏览器一直留在后台。
fn close_browser(flow: &str) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow) {
            if let Some(mut child) = entry.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_dir_all(&entry.profile_dir);
        }
    });
}

// ── 对外的登录 RPC ────────────────────────────────────────────

/// 发起一轮浏览器登录。
pub async fn start(ctx: &PluginCtx, method: &str) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err("WPS 灵犀的「填写凭证」请直接在账号页粘贴 Cookie；浏览器登录请点「网页登录」".to_string());
    }
    // 已经有一轮在跑就复用它：重复点击不该关掉用户正在登录的窗口。
    if let Some(existing) = reusable() {
        agent2api_provider_sdk::log_info!("[WPS 灵犀] 复用进行中的浏览器登录：{existing}");
        return Ok(start_reply(existing));
    }
    close_all();

    let flow = format!("{FLOW_PREFIX}{}", http::now_ms());
    let started_ms = http::now_ms();
    let profile_dir = std::env::temp_dir().join(format!(
        "agent2api-wpslingxi-{}-{started_ms}",
        std::process::id()
    ));
    std::fs::create_dir_all(&profile_dir)
        .map_err(|error| format!("无法创建浏览器临时目录：{error}"))?;

    // `WPSLINGXI_CDP_PORT`：复用一个已经开着调试端口的 Chromium，跳过启动。
    // 给无 GUI 环境 / 自动化测试用；生产里不设置。
    let fixed_port = cdp_override();
    let child = match fixed_port {
        Some(_) => None,
        None => Some(launch(&profile_dir, &login_url())?),
    };
    let hint = if child.is_some() {
        "已打开浏览器窗口，请在里面完成 WPS 账号登录…"
    } else {
        "正在连接浏览器调试端口…"
    };
    with_flows(|table| {
        table.insert(
            flow.clone(),
            Flow {
                child,
                profile_dir: profile_dir.clone(),
                started_ms,
                state: FlowState::Waiting {
                    hint: hint.to_string(),
                },
            },
        );
    });
    spawn_watch(
        flow.clone(),
        profile_dir,
        fixed_port,
        ctx.env.proxy.clone(),
        started_ms + LOGIN_TIMEOUT_MS,
    );

    Ok(start_reply(flow))
}

/// `LoginStart` 只有一处文案要维护，两条入口（网关按钮 / 插件页）都用它。
fn start_reply(flow: String) -> proto::LoginStart {
    proto::LoginStart {
        flow,
        // 浏览器由插件自己拉起，不能再让网关另开一个窗口：那个窗口里的
        // 登录态插件读不到，用户会以为登录没生效。协议允许 url 为空。
        url: String::new(),
        instructions: INSTRUCTIONS.to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        external_browser: false,
        allowed_hosts: Vec::new(),
    }
}

/// 轮询一次进度。拿到 `Captured` 时才真正生成账号（这一步需要 `ctx`
/// 里的网关出口代理，后台任务持有的是一份 clone，别在它里面落账号）。
pub async fn poll(ctx: &PluginCtx, flow: &str) -> Result<proto::LoginPoll, String> {
    if flow.trim().is_empty() {
        return Ok(proto::LoginPoll::Failed {
            message: "缺少登录流程 id，请重新点「网页登录」".to_string(),
        });
    }
    let state = with_flows(|table| table.get(flow).map(|entry| (entry.state.clone(), entry.started_ms)));
    let Some((state, started_ms)) = state else {
        return Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效（插件重启过，或已收尾），请重新发起".to_string(),
        });
    };
    if http::now_ms() - started_ms > LOGIN_TIMEOUT_MS {
        if let Some(mut entry) = take(flow) {
            cleanup(&mut entry);
        }
        return Ok(proto::LoginPoll::Failed {
            message: "登录等待超时（5 分钟），请重新点「网页登录」".to_string(),
        });
    }
    match state {
        FlowState::Waiting { hint } => Ok(proto::LoginPoll::Pending { hint: Some(hint) }),
        FlowState::Failed(message) => {
            if let Some(mut entry) = take(flow) {
                cleanup(&mut entry);
            }
            Ok(proto::LoginPoll::Failed { message })
        }
        FlowState::Captured { cookie, name } => {
            let account = credentials::normalize(
                ctx,
                proto::NormalizeInput {
                    provider: ID.to_string(),
                    source: proto::NormalizeSource::Manual {
                        payload: json!({ "cookie": cookie }),
                    },
                    name,
                    proxy: None,
                },
            )
            .await?;
            if let Some(mut entry) = take(flow) {
                cleanup(&mut entry);
            }
            Ok(proto::LoginPoll::Done { account })
        }
    }
}

/// 取消一轮登录。
pub async fn cancel(flow: &str) -> Result<(), String> {
    if let Some(mut entry) = take(flow) {
        cleanup(&mut entry);
    }
    Ok(())
}

/// 插件自有 UI 页的 action 分流；返回 `None` 表示这不是登录动作，
/// 交给 `credentials::normalize` 按「粘贴 Cookie」处理。
pub async fn ui_dispatch(
    ctx: &PluginCtx,
    payload: &Value,
) -> Option<Result<proto::NormalizedAccount, String>> {
    let action = payload.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        UI_START => Some(match start(ctx, "browser").await {
            Ok(started) => Err(format!(
                "{PREFIX_START}{}|{}",
                started.flow,
                started.instructions.replace('|', " ").replace('\n', " ")
            )),
            Err(error) => Err(error),
        }),
        UI_POLL => {
            let flow = resolve_flow(payload);
            Some(match poll(ctx, &flow).await {
                Ok(proto::LoginPoll::Done { account }) => Ok(account),
                Ok(proto::LoginPoll::Pending { hint }) => Err(format!(
                    "{PREFIX_PENDING}{}",
                    hint.unwrap_or_default().replace('|', " ").replace('\n', " ")
                )),
                Ok(proto::LoginPoll::Failed { message }) => Err(message),
                Err(error) => Err(error),
            })
        }
        UI_CANCEL => {
            let flow = resolve_flow(payload);
            let _ = cancel(&flow).await;
            Some(Err(PREFIX_CANCELLED.to_string()))
        }
        _ => None,
    }
}

/// 插件页带来的 flow id 可能为空、也可能已经失效（网关重启过 / 页面缓存了
/// 旧值）。只要进程里还有一轮在跑就按它继续，别让用户对着已经登录成功的
/// 浏览器窗口重来一次。
fn resolve_flow(payload: &Value) -> String {
    let given = payload
        .get("flow")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !given.is_empty() && alive(&given) {
        return given;
    }
    newest_flow().unwrap_or(given)
}

// ── 后台观察 ──────────────────────────────────────────────────

/// 等浏览器写出调试端口 → 读 cookie → 验登录态 → 落 `Captured`。
///
/// 只读进程内的 flow 表，不碰网关回调：一次登录插件重启后，表本来就没了，
/// 网关下一次 poll 会拿到「流程已失效」，用户重新点一次即可。
fn spawn_watch(
    flow: String,
    profile_dir: PathBuf,
    fixed_port: Option<u16>,
    gateway_proxy: String,
    deadline_ms: i64,
) {
    tokio::spawn(async move {
        while alive(&flow) {
            if http::now_ms() > deadline_ms {
                close_browser(&flow);
                set_state(
                    &flow,
                    FlowState::Failed("登录等待超时（5 分钟），请重新点「网页登录」".to_string()),
                );
                break;
            }
            let port = match fixed_port {
                Some(port) => Some(port),
                None => devtools_port(&profile_dir),
            };
            let outcome = match port {
                None => Observation::Waiting("正在启动浏览器…".to_string()),
                Some(port) => match cdp_cookies(port).await {
                    Err(_) => Observation::Waiting("等待浏览器就绪…".to_string()),
                    Ok(cookies) => observe(&cookies, &gateway_proxy).await,
                },
            };
            match outcome {
                Observation::Waiting(hint) => {
                    set_state(&flow, FlowState::Waiting { hint });
                }
                Observation::Captured { cookie, name } => {
                    set_state(&flow, FlowState::Captured { cookie, name });
                    break;
                }
            }
            tokio::time::sleep(WATCH_INTERVAL).await;
        }
    });
}

enum Observation {
    Waiting(String),
    Captured {
        cookie: String,
        name: Option<String>,
    },
}

/// 一轮 cookie 快照的判定。
///
/// **必须验资料接口**：未登录时 `lingxi.wps.cn` 也会种一份游客 `wps_sid`，
/// 只看到这个名字就收下，会落一条转发必 401 的假账号。
async fn observe(cookies: &[CdpCookie], gateway_proxy: &str) -> Observation {
    let sid = cookie_value(cookies, credentials::SID_KEY);
    if sid.is_empty() {
        return Observation::Waiting("请在打开的浏览器窗口里完成 WPS 账号登录（微信扫码或手机号）…".to_string());
    }
    let uid = cookie_value(cookies, credentials::UID_KEY);
    let branch = cookie_value(cookies, credentials::BRANCH_KEY);
    let credential = credentials::Credential {
        wps_sid: sid.clone(),
        uid: uid.clone(),
        branch: branch.clone(),
    };
    let http_client = http::client(None, gateway_proxy);
    match client::profile(&http_client, &credential, Duration::from_secs(8)).await {
        Ok(profile) if profile_has_identity(&profile) => Observation::Captured {
            cookie: credential.cookie_header(),
            name: display_name_of(&profile),
        },
        _ => Observation::Waiting("已读到登录态，正在确认账号资料…".to_string()),
    }
}

/// 资料接口给的昵称 → 备注名（没有昵称时留给 `normalize` 按 uid 取名）。
fn display_name_of(profile: &Value) -> Option<String> {
    for key in ["name", "nickname", "nickName"] {
        if let Some(text) = profile.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(format!("WPS 灵犀 {trimmed}"));
            }
        }
    }
    None
}

/// 资料接口给出的 `data` 是否像一条真实账号。
/// 游客态通常是 401（在 `client::call` 里就变成 Err），这里再挡一次空响应。
pub fn profile_has_identity(profile: &Value) -> bool {
    for key in ["id", "uid", "userId", "user_id", "name", "nickname"] {
        if let Some(text) = profile.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() && trimmed != "0" {
                return true;
            }
        }
    }
    false
}

// ── CDP ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct CdpCookie {
    name: String,
    value: String,
    domain: String,
}

/// 从浏览器实例的 cookie jar 里取一次快照。
///
/// 走 loopback 的**直连**客户端（`http::client(None, "")` → `no_proxy()`）：
/// 如果让 CDP 请求顺着网关代理出去，会直接连不上 `127.0.0.1`。
async fn cdp_cookies(port: u16) -> Result<Vec<CdpCookie>, String> {
    let http_client = http::client(None, "");
    let response = http_client
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .timeout(CDP_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("浏览器调试端口还没就绪：{error}"))?;
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取浏览器页面列表失败：{error}"))?;
    let targets: Value =
        serde_json::from_str(&text).map_err(|error| format!("浏览器页面列表不是 JSON：{error}"))?;
    let socket_url = targets
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("page"))
                .or_else(|| {
                    items.iter().find(|item| {
                        item.get("webSocketDebuggerUrl")
                            .and_then(Value::as_str)
                            .is_some()
                    })
                })
                .and_then(|item| {
                    item.get("webSocketDebuggerUrl")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        })
        .ok_or_else(|| "浏览器还没有可用的页面".to_string())?;

    let (mut socket, _) = tokio_tungstenite::connect_async(socket_url.as_str())
        .await
        .map_err(|error| format!("连接浏览器调试通道失败：{error}"))?;
    socket
        .send(Message::text(r#"{"id":1,"method":"Storage.getCookies"}"#))
        .await
        .map_err(|error| format!("请求浏览器 Cookie 失败：{error}"))?;
    let mut reply = None;
    for _ in 0..24 {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_i64) == Some(1) {
                    reply = Some(value);
                    break;
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                let _ = socket.send(Message::Pong(payload)).await;
            }
            Some(Ok(_)) => continue,
            _ => break,
        }
    }
    let _ = socket.close(None).await;
    let reply = reply.ok_or_else(|| "浏览器没有返回 Cookie 列表".to_string())?;
    if let Some(error) = reply.get("error") {
        return Err(format!("读取浏览器 Cookie 被拒：{error}"));
    }
    let raw = reply
        .pointer("/result/cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| "浏览器 Cookie 列表形状不对".to_string())?;
    Ok(raw
        .iter()
        .map(|item| CdpCookie {
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            value: item
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            domain: item
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect())
}

/// 取一个 cookie 值。同名 cookie 可能同时挂在 `.wps.cn` 与具体子域上，
/// 优先业务域那份；都没有域信息时取第一个非空值。
fn cookie_value(cookies: &[CdpCookie], name: &str) -> String {
    let candidates: Vec<&CdpCookie> = cookies
        .iter()
        .filter(|item| item.name == name && !item.value.trim().is_empty())
        .collect();
    for item in &candidates {
        if is_wps_domain(&item.domain) {
            return item.value.trim().to_string();
        }
    }
    candidates
        .first()
        .map(|item| item.value.trim().to_string())
        .unwrap_or_default()
}

/// 只认 WPS 自己的域；同名 cookie 挂在第三方域上时不能拿来当登录态。
pub fn is_wps_domain(domain: &str) -> bool {
    let lowered = domain.trim().trim_start_matches('.').to_ascii_lowercase();
    lowered.ends_with("wps.cn") || lowered.ends_with("kdocs.cn")
}

/// `DevToolsActivePort` 第一行是端口，第二行是 browser 级 WS 路径。
pub fn parse_devtools_port(text: &str) -> Option<u16> {
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .and_then(|line| line.parse::<u16>().ok())
        .filter(|port| *port > 0)
}

fn devtools_port(profile_dir: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(profile_dir.join("DevToolsActivePort")).ok()?;
    parse_devtools_port(&text)
}

fn cdp_override() -> Option<u16> {
    std::env::var("WPSLINGXI_CDP_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
}

// ── 启动浏览器 ────────────────────────────────────────────────

fn login_url() -> String {
    std::env::var("WPSLINGXI_LOGIN_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| value.starts_with("https://") || value.starts_with("http://"))
        .unwrap_or_else(|| DEFAULT_LOGIN_URL.to_string())
}

fn launch(profile_dir: &Path, url: &str) -> Result<Child, String> {
    let executable = find_browser()?;
    let mut command = Command::new(&executable);
    command
        // 端口交给 Chrome 自己挑，写进 DevToolsActivePort，避免「先 bind 再
        // 释放」的竞态。
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        // 本地 CDP 客户端不带 Origin 时，新版 Chromium 会拒握手。
        .arg("--remote-allow-origins=*")
        .arg("--new-window")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().map_err(|error| {
        format!(
            "启动浏览器失败（{}）：{error}。也可改用「手动粘贴 Cookie」",
            executable.display()
        )
    })
}

/// 找一个 Chromium 系浏览器。
///
/// `WPSLINGXI_BROWSER` 可显式指定可执行文件（企业定制路径 / 测试替身）。
/// 其余按平台候选逐个试，**不**去动用户日常 profile —— 启动参数里会给一份
/// 全新的临时目录。
fn find_browser() -> Result<PathBuf, String> {
    if let Ok(custom) = std::env::var("WPSLINGXI_BROWSER") {
        let path = PathBuf::from(custom.trim());
        if path.is_file() {
            return Ok(path);
        }
    }
    for candidate in browser_candidates() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("本机没找到 Chrome / Edge / Chromium：请安装任一浏览器，或在「手动粘贴 Cookie」里添加账号".to_string())
}

fn browser_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for key in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Ok(root) = std::env::var(key) {
                let base = PathBuf::from(root);
                out.push(base.join("Google/Chrome/Application/chrome.exe"));
                out.push(base.join("Microsoft/Edge/Application/msedge.exe"));
                out.push(base.join("Chromium/Application/chrome.exe"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
        out.push(PathBuf::from(
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ));
        out.push(PathBuf::from(
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ));
    }
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
        "microsoft-edge-stable",
    ] {
        if let Some(found) = in_path(name) {
            out.push(found);
        }
    }
    out
}

/// 不引 `which` 依赖：PATH 分段拼文件名，Windows 上补 `.exe`。
fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(target_os = "windows")]
        {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devtools_active_port_has_port_on_first_line() {
        assert_eq!(
            parse_devtools_port("9222\n/devtools/browser/abc-123\n"),
            Some(9222)
        );
        assert_eq!(parse_devtools_port("  0\n"), None);
        assert_eq!(parse_devtools_port("not-a-port"), None);
        assert_eq!(parse_devtools_port(""), None);
    }

    #[test]
    fn only_wps_domains_are_trusted() {
        assert!(is_wps_domain(".wps.cn"));
        assert!(is_wps_domain("lingxi.kdocs.cn"));
        assert!(!is_wps_domain("evil-wps.cn.example.com"));
        assert!(!is_wps_domain("localhost"));
    }

    #[test]
    fn business_domain_cookie_wins_over_duplicates() {
        let cookies = vec![
            CdpCookie {
                name: credentials::SID_KEY.to_string(),
                value: "from-third-party".to_string(),
                domain: "tracker.example.com".to_string(),
            },
            CdpCookie {
                name: credentials::SID_KEY.to_string(),
                value: "real-sid".to_string(),
                domain: ".wps.cn".to_string(),
            },
        ];
        assert_eq!(cookie_value(&cookies, credentials::SID_KEY), "real-sid");
        assert_eq!(cookie_value(&cookies, credentials::UID_KEY), "");
    }

    #[test]
    fn profile_must_look_like_a_real_account() {
        assert!(profile_has_identity(&json!({ "id": "12345", "name": "测试账号" })));
        assert!(profile_has_identity(&json!({ "nickname": "灵犀用户" })));
        assert!(!profile_has_identity(&json!({})));
        assert!(!profile_has_identity(&json!({ "id": "0" })));
    }

    #[test]
    fn display_name_uses_the_profile_nickname() {
        assert_eq!(
            display_name_of(&json!({ "id": "1", "name": "灵犀用户" })),
            Some("WPS 灵犀 灵犀用户".to_string())
        );
        assert_eq!(display_name_of(&json!({ "id": "1" })), None);
    }

    #[test]
    fn default_login_url_is_https_and_matches_the_upstream_host() {
        // 不碰进程级环境变量（并行单测会互相污染）：只验常量本身的形状。
        assert!(DEFAULT_LOGIN_URL.starts_with("https://"));
        assert_eq!(
            crate::wpslingxi::endpoints::DEFAULT_BASE,
            "https://lingxi.wps.cn"
        );
    }
}
