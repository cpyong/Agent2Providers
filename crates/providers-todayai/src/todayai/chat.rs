//! 会话式转发（`stateful`）：把 OpenAI Chat 请求翻译成 Today 的**个人助理
//! 会话**协议。
//!
//! ── 为什么不是无状态代理（这条最要紧）────────────────────────
//! `api.todayai.cn` **没有** OpenAI / Anthropic 兼容的推理接口：
//! `/v1/chat/completions`、`/v1/responses`、`/v1/llm/chat/completions`
//! 全部 404，唯一与模型相关的是目录 `/v1/llm/models`。真正跑推理的是
//! **助理会话**（v3 协议），它不是一个「一次请求 = 一次补全」的接口：
//!
//! ```text
//!   POST /v3/conversations                       → conversationId（24h）
//!   POST /v3/messages                            → 202/200，落一条用户消息
//!   GET  /v3/conversations/{id}/message-changes  → 长轮询，取助理回答
//!   PATCH /v1/users/me/settings {preferredModel} → 选模型（账号级）
//! ```
//!
//! 所以这里申报 `stateful`，由插件自己驱动这三步，再把结果翻译成 OpenAI
//! 的 `chat.completion.chunk` 推给网关。
//!
//! ── 每次请求一个新会话（隐私与卫生）──────────────────────────
//! 不复用主会话：调用方的 prompt 不会落进用户的 Today 聊天时间线，也不会
//! 把主会话的历史带进上下文。代价是**没有多轮记忆** —— 多轮由客户端自己
//! 在 `messages` 里带过来，这里把历史压成一段上下文（见 [`prompt_of`]）。
//!
//! ── 仍然要说清的三件事（不是缺陷，是上游形态）─────────────────
//!   1. **回答来自用户的个人助理**：它带着用户的记忆（`/v1/agents/default`
//!      里的 userFacts）与已连接的连接器。插件无法关掉这些，能做的只是
//!      「每次开新会话」；
//!   2. **模型是账号级设置**：上游没有 per-message 的模型字段，只能用
//!      `settings.preferredModel`。它是**全局**的，所以同一账号的请求必须
//!      串行（见 [`account_lock`]），否则两个并发请求会互相改掉对方的模型；
//!   3. **没有增量**：`message-changes` 给的是**整条**消息（不是 token 增量），
//!      因此流式响应只有一帧正文。客户端仍会正常收到 `finish_reason` 与
//!      `[DONE]`，只是「打字机效果」不存在。
//!
//! ── 工具调用 / 思考位：不支持 ────────────────────────────────
//! 上游返回的是助理的成品消息，没有 `tool_calls`，也没有独立的思考流。
//! 目录里因此把 `supportsToolCall` / `supportsReasoning` 一律按 false 申报
//! （见 `models.rs`），免得客户端把工具定义发过来却永远等不到调用。
//!
//! ── 软拒绝：HTTP 200 + 一句固定文案 ─────────────────────────
//! 上游对「它不想接的输入」不报错，而是正常落一条助理消息：
//!
//! ```text
//! 我暂时无法处理这条消息，请稍后重试或换个话题。
//! ```
//!
//! 这是**拒答**，不是回答。实测触发面很宽：把 Cursor 的
//! `<user_info>` / `<rules>`（用户自定义规则，十几 KB 的祈使句）原样塞进
//! `body.text` 时几乎必现；纯对话历史到四五万字符也开始出现。原样转出去
//! 的后果是客户端把它当成模型输出，用户只看到一句莫名其妙的话术，重试
//! 也永远一样。所以这里做三件事：
//!
//!   1. [`sanitize`] 剥掉客户端注入的元数据段（`<git_status>` /
//!      `<agent_skills>` / `<mcp_meta_tools>` …），规则段只留开头一小截；
//!   2. [`prompt_of`] 给整段提示词设字符预算，历史从近到远装到装满为止；
//!   3. [`await_answer`] 认出这句拒答就**报错**（带 `REFUSAL_PREFIX`），
//!      `run` 再拿一份「只带最近两轮」的精简提示词重试一次；仍然拒答才
//!      把原因如实告诉客户端。
//!
//! 一句话：宁可让客户端看到一次可重试的错误，也不要把拒答伪装成回答。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};
use serde_json::{json, Value};

use super::{client, credentials, endpoints};
use crate::http;

/// 单次上游调用的超时（长轮询本身要挂 25 秒，20 秒的控制面超时不够）
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// 长轮询的挂起上限（上游 `longPollWaitMaxMs` = 25000）
const POLL_WAIT_MS: i64 = 25_000;

/// 收到回答后再探一次的等待：确认助理没有续写同一句话
const SETTLE_WAIT_MS: i64 = 1_200;

/// 等回答的总预算（从发出消息算起）
const ANSWER_BUDGET_MS: i64 = 150_000;

/// 一次请求送上游的字符预算。
///
/// 实测口径（2026-11，同一账号、同一模型）：四万四千字符的普通对话历史
/// 可以正常回答；带 Cursor `<rules>` 的输入三千字符就开始拒答，十几 KB
/// 的完整 `<user_info>` 必拒。取 3 万作上限，是「正常多轮历史能装下、
/// 又不贴着红线」的折中 —— 上限只是护栏，真正的杀手是元数据段（见
/// [`sanitize`]）。
const PROMPT_BUDGET_CHARS: usize = 30_000;

/// 当前这一问的保留上限（宁可砍历史，也不砍用户正在问的东西）。
const CURRENT_BUDGET_CHARS: usize = 12_000;

/// 单条历史消息的保留上限（工具结果动辄两万字，全留会把预算吃光）。
const TURN_BUDGET_CHARS: usize = 4_000;

/// 客户端规则段（`<rules>` / `<user_rule>`）的保留上限。
///
/// 用户规则常常是十几 KB 的「必须 / 禁止」祈使句，实测超过一千多字符
/// 上游就整段拒答。保留开头是因为引用格式这类硬要求通常写在最前面；
/// 被截断的部分换来的是「至少能答」。
const RULES_BUDGET_CHARS: usize = 1_200;

/// 精简重试时保留的历史轮数。
const RETRY_TURNS: usize = 2;

/// 上游软拒绝的错误前缀（`run` 据此决定要不要精简重试）。
const REFUSAL_PREFIX: &str = "TODAYAI_REFUSAL:";

/// 上游的拒答话术（HTTP 200，正文就是这一句）。
const REFUSAL_MARKERS: &[&str] = &[
    "我暂时无法处理这条消息",
    "请稍后重试或换个话题",
    "暂时无法处理这条消息",
];

/// 拒答话术的最大长度：超过这个长度就当成正常回答，避免误伤正文里
/// 恰好引用了这句话的回复。
const REFUSAL_MAX_CHARS: usize = 200;

/// 入口：跑完「选模型 → 建会话 → 发消息 → 等回答 → 推 SSE」。
pub async fn run(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    let prompt = prompt_of(&req.body, Plan::Full)?;
    let model = model_of(&req);
    let account = req.account.clone();

    // 模型是账号级设置，同一账号必须串行（见模块头第 2 条）。
    let lock = account_lock(&account).await?;
    let _serialized = lock.lock().await;

    let http_client = http::client(req.proxy.as_ref(), &ctx.env.proxy);
    let mut token = credentials::access_token(ctx, &account).await?;

    if !model.is_empty() {
        apply_model(ctx, &http_client, &mut token, &account, &model).await?;
    }

    let conversation = reserve(ctx, &http_client, &mut token, &account).await?;
    append(
        ctx,
        &http_client,
        &mut token,
        &account,
        &conversation,
        &prompt,
    )
    .await?;
    let answer = match await_answer(ctx, &http_client, &mut token, &account, &conversation).await {
        Ok(answer) => answer,
        Err(error) if error.starts_with(REFUSAL_PREFIX) => {
            // 上游拒答：换一份只带最近两轮的精简提示词，在**新会话**里再试一次。
            // 不沿用旧会话 —— 那里的第一条用户消息已经落库，再发一条会变成
            // 「追问」，模型反而更容易顺着前文的元数据继续拒答。
            let short = prompt_of(&req.body, Plan::Retry)?;
            agent2api_provider_sdk::log_warn!(
                "[Today AI] 上游拒答（{} 字），改用精简提示词重试（{} 字）",
                prompt.chars().count(),
                short.chars().count()
            );
            let retry = reserve(ctx, &http_client, &mut token, &account).await?;
            append(ctx, &http_client, &mut token, &account, &retry, &short).await?;
            match await_answer(ctx, &http_client, &mut token, &account, &retry).await {
                Ok(answer) => answer,
                Err(again) if again.starts_with(REFUSAL_PREFIX) => {
                    return Err(refusal_hint());
                }
                Err(other) => return Err(other),
            }
        }
        Err(error) => return Err(error),
    };

    emit(&mut sink, &model, &answer).await
}

/// 两次都拒答时的对外错误。
///
/// 这条消息是给**用户**看的，要把「为什么」和「怎么办」说清：不是网络
/// 问题、不是额度问题，而是上游助理不接受这次输入。
fn refusal_hint() -> String {
    "Today AI 拒绝了这次请求（上游返回「我暂时无法处理这条消息」，精简重试后仍然拒绝）。\
     常见原因是客户端把大段自定义规则（Cursor Rules）或工具/技能清单塞进了提示词，\
     Today 的助理会判定为不可处理的输入。可以：换一个模型再试、精简 Cursor 规则后重试，\
     或直接在 Today AI 官方客户端里提问"
        .to_string()
}

// ─── 请求体 → 一段 prompt ───────────────────────────────────

/// 提示词的两种档位：完整（首轮）与精简（拒答后的重试）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 首轮：清洗 + 预算内的全部历史
    Full,
    /// 重试：只带最近 [`RETRY_TURNS`] 轮，且每条都截短
    Retry,
}

/// 整段丢弃的客户端元数据标签。
///
/// 这些是 Cursor 给**自家模型**准备的上下文（git 快照、技能清单、MCP 工具
/// 目录、历史会话路径…），对本家助理没有语义价值，却实实在在地把提示词
/// 撑到十几 KB，并且是拒答的主要诱因。丢掉它们不损失用户真正问的东西。
const DROP_TAGS: &[&str] = &[
    "git_status",
    "agent_transcripts",
    "agent_skills",
    "available_skills",
    "mcp_meta_tools",
    "mcp_meta_tool_servers",
    "open_and_recently_viewed_files",
    "conversation_summary",
    "tools",
];

/// 规则标签：保留但限长（见 [`RULES_BUDGET_CHARS`]）。
const RULES_TAGS: &[&str] = &["rules", "user_rule"];

/// `<user_info>` 的保留上限：OS / Shell / 工作区路径都在最前面，够用了。
const USER_INFO_BUDGET_CHARS: usize = 600;

/// 把一份消息文本清洗成「助理能读」的样子。
///
/// 顺序有讲究：先削规则段（它可能嵌在 `<user_info>` 里面），再剥其余元数据
/// 段，最后收窄 `<user_info>` 的剩余部分 —— 反过来做会把嵌套的规则段整段
/// 带走，用户写的引用格式要求就彻底没了。
pub fn sanitize(text: &str) -> String {
    let mut out = text.to_string();
    for tag in RULES_TAGS {
        out = limit_block(&out, tag, RULES_BUDGET_CHARS);
    }
    for tag in DROP_TAGS {
        out = remove_block(&out, tag);
    }
    limit_block(&out, "user_info", USER_INFO_BUDGET_CHARS)
}

/// 删掉 `<tag>…</tag>` 整段（没有闭合标签时丢到结尾）。
fn remove_block(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        match after.find(&close) {
            Some(end) => rest = &after[end + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 把 `<tag>…</tag>` 的**内容**截到 `budget` 字，标签本身保留。
fn limit_block(text: &str, tag: &str, budget: usize) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len().min(budget + 256));
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        match after.find(&close) {
            Some(end) => {
                let body = after[..end].trim();
                out.push_str(&open);
                out.push_str(&clip_chars(body, budget));
                out.push_str(&close);
                rest = &after[end + close.len()..];
            }
            None => {
                // 没有闭合：原样留着，交给后面的预算裁剪
                out.push_str(&open);
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 按**字符**截断并标注（不能按字节切，中文会切出半个字符）。
pub fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str(" …（已截断）");
    out
}

/// 把 OpenAI `messages` 压成一段纯文本。
///
/// ── 为什么要压 ──────────────────────────────────────────────
/// 上游的 `POST /v3/messages` 只收 `body.text`（单段文本），没有角色数组。
/// 单条 user 消息原样发；多轮则把历史写在前面、当前问题写在最后 —— 这样
/// 助理看到的是「上下文 + 这一问」，而不是把 `assistant:` 也当成用户说的。
///
/// ── 预算怎么花 ──────────────────────────────────────────────
/// 当前问题先占位（[`CURRENT_BUDGET_CHARS`]），剩下的额度从**最近**的历史
/// 往前装，每条先截到 [`TURN_BUDGET_CHARS`]；装不下的就整条跳过并在开头
/// 记一句「更早的 N 轮已省略」，让模型知道上下文不完整，而不是以为对话
/// 就这么短。
pub fn prompt_of(body: &Value, plan: Plan) -> Result<String, String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "Today AI 只支持 chat/completions 的 messages 请求体".to_string())?;
    let turns: Vec<(String, String)> = messages
        .iter()
        .filter_map(|message| {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .trim()
                .to_string();
            let text = message_text(message)?;
            let cleaned = sanitize(&text);
            if cleaned.trim().is_empty() {
                return None;
            }
            Some((role, cleaned))
        })
        .collect();
    let Some((last, history)) = turns.split_last() else {
        return Err("请求里没有可发送的文本内容".to_string());
    };

    let current = clip_chars(last.1.trim(), CURRENT_BUDGET_CHARS);
    let limit = match plan {
        Plan::Full => usize::MAX,
        Plan::Retry => RETRY_TURNS,
    };
    let mut used = current.chars().count();
    let mut picked: Vec<(&str, String)> = Vec::new();
    let mut dropped = 0_usize;
    for (role, text) in history.iter().rev() {
        if picked.len() >= limit {
            dropped += 1;
            continue;
        }
        let piece = match plan {
            Plan::Full => clip_chars(text, TURN_BUDGET_CHARS),
            Plan::Retry => clip_chars(text, TURN_BUDGET_CHARS / 4),
        };
        let cost = piece.chars().count();
        if used + cost > PROMPT_BUDGET_CHARS {
            dropped += 1;
            continue;
        }
        used += cost;
        picked.push((role.as_str(), piece));
    }
    picked.reverse();

    if picked.is_empty() {
        return Ok(current);
    }
    let mut prompt = String::from("以下是本次对话的历史，仅作上下文：\n");
    if dropped > 0 {
        prompt.push_str(&format!("（更早的 {dropped} 条消息已省略）\n"));
    }
    for (role, text) in &picked {
        prompt.push_str(role);
        prompt.push_str(": ");
        prompt.push_str(text);
        prompt.push('\n');
    }
    prompt.push_str("\n请回答最后这条消息：\n");
    if last.0 != "user" {
        prompt.push_str(&last.0);
        prompt.push_str(": ");
    }
    prompt.push_str(&current);
    Ok(prompt)
}

/// 取一条消息的文本：`content` 是字符串，或是 `[{type:"text", text}]` 数组。
fn message_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let parts = content.as_array()?;
    let mut out = String::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 网关解析出的发送名优先（`model_rules` 改写之后），其次才是 body 里的原名。
fn model_of(req: &proto::ConversationRequest) -> String {
    let wire = req.wire_model.trim();
    if !wire.is_empty() {
        return wire.to_string();
    }
    req.body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

// ─── 模型（账号级设置）──────────────────────────────────────

/// 已设置过的模型（账号 → 模型名）。命中就不必每轮 PATCH 一次。
fn applied_models() -> &'static Mutex<HashMap<String, String>> {
    static APPLIED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    APPLIED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 把账号的 `preferredModel` 设成这次请求要的模型。
///
/// 上游对未知模型回 400（实测），所以这里如实报错而不是「悄悄用默认模型」
/// —— 后者会让客户端以为自己在跟 `gpt-6-astra` 说话，其实不是。
async fn apply_model(
    ctx: &PluginCtx,
    http_client: &reqwest::Client,
    token: &mut String,
    account: &str,
    model: &str,
) -> Result<(), String> {
    let cached = applied_models()
        .lock()
        .ok()
        .and_then(|guard| guard.get(account).cloned());
    if cached.as_deref() == Some(model) {
        return Ok(());
    }
    let url = format!("{}/v1/users/me/settings", endpoints::api_base_url());
    let body = json!({ "settings": { "preferredModel": model } });
    let (status, payload) = authed_request(
        ctx,
        http_client,
        token,
        account,
        reqwest::Method::PATCH,
        &url,
        Some(&body),
        &[],
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(client::describe_error(
            status,
            &payload,
            &format!("设置模型「{model}」"),
        ));
    }
    if let Ok(mut guard) = applied_models().lock() {
        guard.insert(account.to_string(), model.to_string());
    }
    Ok(())
}

// ─── 三步会话协议 ──────────────────────────────────────────

/// `POST /v3/conversations`：占一个只属于这次请求的会话。
async fn reserve(
    ctx: &PluginCtx,
    http_client: &reqwest::Client,
    token: &mut String,
    account: &str,
) -> Result<String, String> {
    let url = format!("{}/v3/conversations", endpoints::api_base_url());
    let extra = [(
        "Idempotency-Key".to_string(),
        format!("agent2api-{}", unique_id()),
    )];
    let (status, payload) = authed_request(
        ctx,
        http_client,
        token,
        account,
        reqwest::Method::POST,
        &url,
        None,
        &extra,
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(client::describe_error(status, &payload, "创建 Today 会话"));
    }
    payload
        .get("data")
        .and_then(|data| data.get("conversation"))
        .and_then(|conversation| conversation.get("conversationId"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "创建 Today 会话失败：上游没有返回 conversationId".to_string())
}

/// `POST /v3/messages`：把 prompt 落进会话。
async fn append(
    ctx: &PluginCtx,
    http_client: &reqwest::Client,
    token: &mut String,
    account: &str,
    conversation: &str,
    prompt: &str,
) -> Result<(), String> {
    let url = format!("{}/v3/messages", endpoints::api_base_url());
    let body = json!({
        "conversationId": conversation,
        "clientMessageId": format!("agent2api-{}", unique_id()),
        "bodyType": "text",
        "body": { "text": prompt },
    });
    let (status, payload) = authed_request(
        ctx,
        http_client,
        token,
        account,
        reqwest::Method::POST,
        &url,
        Some(&body),
        &[],
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(client::describe_error(status, &payload, "发送 Today 消息"));
    }
    Ok(())
}

/// 长轮询 `message-changes`，直到助理那条消息稳定下来。
///
/// ── 为什么收到回答还要再探一次 ───────────────────────────────
/// v3 的变更项是**整条消息快照**（不是增量），但协议没有 `isFinal` 之类的
/// 完成位。收到一条助理消息后再用很短的 `waitMs` 探一次：没有新变更就说明
/// 这条回答已经写完；有新变更（同一 `id`、更长的正文）就用新的。
async fn await_answer(
    ctx: &PluginCtx,
    http_client: &reqwest::Client,
    token: &mut String,
    account: &str,
    conversation: &str,
) -> Result<String, String> {
    if !conversation
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err("上游返回的 conversationId 含有无法拼进 URL 的字符".to_string());
    }
    let base = endpoints::api_base_url();
    let started = http::now_ms();
    let mut after = 0_i64;
    let mut answer = String::new();
    loop {
        if http::now_ms() - started > ANSWER_BUDGET_MS {
            return Err(if answer.is_empty() {
                "等待 Today AI 回答超时：上游在 150 秒内没有给出回复".to_string()
            } else {
                "等待 Today AI 回答超时：回复在 150 秒内没有写完".to_string()
            });
        }
        let wait = if answer.is_empty() {
            POLL_WAIT_MS
        } else {
            SETTLE_WAIT_MS
        };
        let url = format!(
            "{base}/v3/conversations/{conversation}/message-changes?afterSequence={after}&waitMs={wait}"
        );
        let (status, payload) = authed_request(
            ctx,
            http_client,
            token,
            account,
            reqwest::Method::GET,
            &url,
            None,
            &[],
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(client::describe_error(status, &payload, "读取 Today 回答"));
        }
        let mut updated = false;
        let items = payload
            .get("data")
            .and_then(|data| data.get("items"))
            .and_then(Value::as_array);
        if let Some(items) = items {
            for item in items {
                if let Some(sequence) = item.get("sequence").and_then(Value::as_i64) {
                    if sequence > after {
                        after = sequence;
                    }
                }
                if item.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                if let Some(text) = message_body_text(item) {
                    if !text.trim().is_empty() && text != answer {
                        answer = text;
                        updated = true;
                    }
                }
            }
        }
        if !answer.is_empty() && !updated {
            // 上游的「软拒绝」是 HTTP 200 + 一句固定话术，必须在这里拦住：
            // 原样返回等于把拒答伪装成回答，客户端和用户都无从判断。
            if let Some(reason) = refusal_of(&answer) {
                return Err(reason);
            }
            return Ok(answer);
        }
    }
}

/// 判断一条助理消息是不是上游的拒答话术。
///
/// 只认「短消息里出现固定句式」：长回复里恰好引用了这句话（比如用户在问
/// 「为什么总说暂时无法处理」）不该被当成拒答。
fn refusal_of(answer: &str) -> Option<String> {
    let trimmed = answer.trim();
    if trimmed.chars().count() > REFUSAL_MAX_CHARS {
        return None;
    }
    if REFUSAL_MARKERS.iter().any(|marker| trimmed.contains(marker)) {
        return Some(format!("{REFUSAL_PREFIX}{trimmed}"));
    }
    None
}

/// v3 消息体的正文：`body.content.parts[].text`，兼容 `body.content.text`。
fn message_body_text(item: &Value) -> Option<String> {
    let body = item.get("body")?;
    let content = body.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    let parts = content.get("parts").and_then(Value::as_array)?;
    let mut out = String::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ─── 出站：OpenAI chat SSE ─────────────────────────────────

/// 推 head + 三帧 chunk + `[DONE]`。
///
/// 非流式客户端由网关自己聚合（`aggregate_frame_stream`），所以插件只写
/// 这一条路径 —— 两条路径的正文迟早会分叉。
async fn emit(
    sink: &mut ConversationSink<'_>,
    model: &str,
    answer: &str,
) -> Result<(), String> {
    let mut headers = BTreeMap::new();
    headers.insert("content-type".to_string(), "text/event-stream".to_string());
    sink.head(proto::StreamHead {
        status: 200,
        headers,
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;

    let id = format!("chatcmpl-{}", unique_id());
    let created = http::now_ms() / 1000;
    let model_out = if model.is_empty() { "todayai" } else { model };
    let frames = [
        json!({
            "id": id.clone(),
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_out,
            "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" }, "finish_reason": null }],
        }),
        json!({
            "id": id.clone(),
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_out,
            "choices": [{ "index": 0, "delta": { "content": answer }, "finish_reason": null }],
        }),
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_out,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        }),
    ];
    for frame in frames {
        sink.send(format!("data: {frame}\n\n").as_bytes()).await?;
    }
    sink.send(b"data: [DONE]\n\n").await?;
    Ok(())
}

// ─── 鉴权与串行 ────────────────────────────────────────────

/// 带 Bearer 的一次请求；401 时用会话令牌换一枚再重试一次。
///
/// 401 的自愈在这里做而不是交给网关：有状态通道里「上游回 401」发生在
/// **插件内部**，网关看到的只是一条错误字符串，分类器没有机会介入。
#[allow(clippy::too_many_arguments)]
async fn authed_request(
    ctx: &PluginCtx,
    http_client: &reqwest::Client,
    token: &mut String,
    account: &str,
    method: reqwest::Method,
    url: &str,
    body: Option<&Value>,
    extra: &[(String, String)],
) -> Result<(u16, Value), String> {
    let (status, payload) =
        send_once(http_client, token, method.clone(), url, body, extra).await?;
    if status != 401 {
        return Ok((status, payload));
    }
    *token = credentials::refresh_token(ctx, account).await?;
    send_once(http_client, token, method, url, body, extra).await
}

async fn send_once(
    http_client: &reqwest::Client,
    token: &str,
    method: reqwest::Method,
    url: &str,
    body: Option<&Value>,
    extra: &[(String, String)],
) -> Result<(u16, Value), String> {
    let mut headers = client::api_headers();
    headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    for (key, value) in extra {
        headers.push((key.clone(), value.clone()));
    }
    http::request_json(http_client, method, url, &headers, body, REQUEST_TIMEOUT).await
}

/// 账号级串行锁（模型是账号级设置，见模块头）。
async fn account_lock(account: &str) -> Result<Arc<tokio::sync::Mutex<()>>, String> {
    let key = if account.trim().is_empty() {
        "-".to_string()
    } else {
        account.to_string()
    };
    {
        let guard = account_locks()
            .lock()
            .map_err(|_| "Today AI 账号锁表已损坏".to_string())?;
        if let Some(existing) = guard.get(&key) {
            return Ok(existing.clone());
        }
    }
    let mut guard = account_locks()
        .lock()
        .map_err(|_| "Today AI 账号锁表已损坏".to_string())?;
    Ok(guard
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone())
}

fn account_locks() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 进程内唯一的短 id（`Idempotency-Key` / `chatcmpl-` / `clientMessageId`）。
///
/// 不引 uuid 依赖：时间戳 + pid + 自增序号在本进程内已经唯一，而这三个
/// 用途都只要求「同一次请求不与别的请求撞车」。
fn unique_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:x}{:x}{:x}",
        http::now_ms() as u64,
        std::process::id(),
        seq
    )
}

#[cfg(test)]
mod tests {
    // 测试里 expect 失败就是失败，正是想要的行为；crate 级的 `deny` 留给生产代码
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn text_of(s: &str) -> String {
        sanitize(s)
    }

    #[test]
    fn drops_cursor_metadata_blocks() {
        let raw = "<user_info>\nOS Version: win32\n</user_info>\n<git_status>lots of git\n</git_status>\n<agent_skills>skill list\n</agent_skills>\n真正的提问：修一下这个 bug";
        let out = text_of(raw);
        assert!(out.contains("OS Version"), "user_info 的头部要留下");
        assert!(!out.contains("lots of git"), "git_status 要整段丢掉");
        assert!(!out.contains("skill list"), "技能清单要整段丢掉");
        assert!(out.contains("修一下这个 bug"), "用户的话必须留下");
    }

    #[test]
    fn limits_user_rules() {
        let long_rule = "必须给出可点击引用。".repeat(500);
        let raw = format!("<rules><user_rule>{long_rule}</user_rule></rules>\n问题：继续");
        let out = text_of(&raw);
        assert!(out.chars().count() < raw.chars().count());
        assert!(out.contains("已截断"));
        assert!(out.contains("问题：继续"));
    }

    #[test]
    fn keeps_user_info_head_only() {
        let raw = format!("<user_info>前情{}尾巴</user_info>", "填充".repeat(1000));
        let out = text_of(&raw);
        assert!(out.chars().count() < 800);
        assert!(out.contains("前情"));
    }

    #[test]
    fn prompt_fits_budget() {
        let mut messages = Vec::new();
        for index in 0..200 {
            messages.push(serde_json::json!({
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "content": "代码片段".repeat(800),
            }));
        }
        messages.push(serde_json::json!({ "role": "user", "content": "最后这一问" }));
        let body = serde_json::json!({ "messages": messages });
        let prompt = prompt_of(&body, Plan::Full).expect("prompt");
        assert!(prompt.chars().count() <= PROMPT_BUDGET_CHARS + 64);
        assert!(prompt.contains("最后这一问"));
        assert!(prompt.contains("已省略"), "被丢掉的历史要如实说明");
    }

    #[test]
    fn retry_plan_keeps_only_tail() {
        let body = serde_json::json!({
            "messages": [
                { "role": "user", "content": "第一轮问题" },
                { "role": "assistant", "content": "第一轮回答" },
                { "role": "user", "content": "第二轮问题" },
                { "role": "assistant", "content": "第二轮回答" },
                { "role": "user", "content": "第三轮问题" },
                { "role": "assistant", "content": "第三轮回答" },
                { "role": "user", "content": "当前这一问" },
            ]
        });
        let prompt = prompt_of(&body, Plan::Retry).expect("prompt");
        assert!(prompt.contains("第三轮回答"), "重试档要带上最近一轮");
        assert!(!prompt.contains("第二轮问题"), "重试档只留最近两轮");
        assert!(!prompt.contains("第一轮问题"));
    }

    #[test]
    fn refusal_detected() {
        assert!(refusal_of("我暂时无法处理这条消息，请稍后重试或换个话题。").is_some());
    }

    #[test]
    fn long_answer_not_refusal() {
        let mut long = "我暂时无法处理这条消息".to_string();
        long.push_str(&"补充说明".repeat(100));
        assert!(refusal_of(&long).is_none(), "长回复里引用这句话不算拒答");
    }

    #[test]
    fn clip_is_char_safe() {
        let text = "中".repeat(100);
        let out = clip_chars(&text, 10);
        assert!(out.starts_with("中中中中中中中中中中"));
        assert!(out.ends_with("已截断）"));
    }
}
