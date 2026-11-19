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
//! ── 工具调用：文本协议桥接（不是上游原生）────────────────────
//! 上游返回的是助理的成品消息，没有原生 `tool_calls`。但 Cursor 的 Agent
//! 模式没有「关掉工具」的开关 —— 读文件、改文件、跑命令全靠 function
//! calling，它无条件带 `tools`。插件若原样忽略，模型根本看不到工具，只能
//! 回一句「我先看一下…」，Agent 循环随即结束（一轮就收工）。
//!
//! 所以这里做与 `tools/tool-bridge/bridge.mjs` 相同的桥接：
//!   1. 上行把 `body.tools` 压成一段文本协议（工具名 + 裁剪后的 schema），
//!      要求模型需要调用时只输出
//!      `<tool_call>{"name":"…","arguments":{…}}</tool_call>`；
//!   2. 历史里的 `assistant.tool_calls` / `role:"tool"` 摊平成文本，保留
//!      「调过什么、拿到什么」，下一轮还能接着推理；
//!   3. 下行把模型吐出的块还原成标准 `tool_calls` 帧，并以
//!      `finish_reason:"tool_calls"` 收尾 —— 客户端据此继续下一轮。
//!
//! 可靠性取决于模型对协议的遵从度：解析失败就丢弃该块、当普通文本回答，
//! 绝不伪造一个可能改坏文件或执行错命令的调用。思考位仍按 false 申报。
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

/// 收到回答后的收尾等待：确认助理没有续写同一条消息。
///
/// 上游没有 `isFinal` 完成位，只能靠「静默」推断写完。1.2 秒太激进：
/// 模型说完「我先看一下…」再生成 `<tool_call>` 的 JSON 时，中间的空档
/// 常常超过一秒，插件会抢在调用块之前收工 —— 客户端看到的就是任务突然
/// 结束。改成 3 秒挂起 + 连续两次静默（见 [`SETTLE_QUIET_PROBES`]）。
const SETTLE_WAIT_MS: i64 = 3_000;

/// 判定「写完了」需要的连续静默次数。
///
/// 一次静默可能只是模型在思考下一步，两次（约 6 秒）才当作真的结束。
/// 回答里还有未闭合的 `<tool_call>` 时无论静默多久都不算完。
const SETTLE_QUIET_PROBES: usize = 2;

/// 正文里还有未闭合 `<tool_call>` 时允许的静默次数上限。
///
/// 比正常收尾多给几次窗口（调用块的 JSON 可能很长）；到顶仍不闭合就是
/// 标签写坏了，收工交给 `parse_tool_calls` 的容错分支处理。
const OPEN_BLOCK_GRACE_PROBES: usize = 5;

/// 「空转」判定：只有计划、没有动作的回复最多这么长。
///
/// 真正的最终答案通常更长；超过这个长度还只谈计划的，多半是在解释
/// 为什么没法继续，追问也救不回来，不如原样交给用户。
const PLAN_MAX_CHARS: usize = 600;

/// 模型「只说不做」时追加的纠正指令。
///
/// 发在**同一会话**里：上文（含工具结果）已经在会话里，指令本身可以很短，
/// 不必重发整份提示词。
const NUDGE_PROMPT: &str = "[SYSTEM CORRECTION] Your previous reply only announced what you were \
about to do and contained no <tool_call> block, so the client ended the task. Emit the <tool_call> \
block for the next step now. Do not explain, do not apologise, do not restate the plan. \
If the task is genuinely finished, say so in one sentence and give the final answer.";

/// 「只说不做」的句子特征：说了要去看/要改，却没有给调用块。
///
/// 命中后追加 [`NUDGE_PROMPT`] 再问一轮。保守起见只在短回复里判定 ——
/// 长回复里的「我先」往往只是在叙述步骤。
const PLAN_MARKERS: &[&str] = &[
    "我先看一下",
    "我先看",
    "我先读",
    "我先确认",
    "让我先",
    "让我看",
    "我先查",
    "我需要先",
    "我需要查看",
    "接下来我",
    "下一步我",
    "我先了解",
    "i will look",
    "i'll look",
    "let me look",
    "i will check",
    "i'll check",
    "let me check",
    "i will read",
    "i'll read",
    "let me read",
];

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

/// 单个工具描述的保留上限（工具清单太长会挤掉对话历史，也更容易触发拒答）。
const TOOL_DESC_LIMIT: usize = 200;

/// 工具参数 schema 的递归深度上限（Cursor 的 schema 里有大量嵌套描述）。
const TOOL_SCHEMA_DEPTH: usize = 5;

/// 一次注入的工具数量上限：超出的按顺序丢弃，并在协议块里说明。
const MAX_TOOLS: usize = 40;

/// 工具协议的字符上限。超过就停止追加工具，保证正文还有预算。
const TOOL_PROTOCOL_MAX_CHARS: usize = 12_000;

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
    // 工具清单只在这次请求里有效：上行用它注入协议，下行用它校验调用名。
    let tools = tools_of(&req.body);
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
    let (answer, sequence) = answer;
    agent2api_provider_sdk::log_info!(
        "[Today AI] 请求完成：模型 {} / 工具 {} 个 / 提示词 {} 字 / 回答 {} 字",
        if model.is_empty() { "默认" } else { &model },
        tools.len(),
        prompt.chars().count(),
        answer.chars().count()
    );

    // ── 空转兜底：模型只说了「我先看一下」，没给调用块 ──────────
    // 文本协议不是原生 function calling，遵从度取决于模型。客户端拿到
    // `finish_reason:"stop"` 就收工，表现为任务突然结束 —— 这里补一次
    // 同会话追问，把「计划」逼成「动作」。
    let answer = if tools.is_empty() {
        answer
    } else {
        let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
        let (calls, _) = parse_tool_calls(&answer, &names);
        if calls.is_empty() && looks_like_plan(&answer) {
            agent2api_provider_sdk::log_warn!(
                "[Today AI] 回答只有计划没有工具调用（{} 字），追加纠正指令重问一轮",
                answer.chars().count()
            );
            append(
                ctx,
                &http_client,
                &mut token,
                &account,
                &conversation,
                NUDGE_PROMPT,
            )
            .await?;
            match await_answer_after(
                ctx,
                &http_client,
                &mut token,
                &account,
                &conversation,
                sequence,
            )
            .await
            {
                // 追问后仍只有计划：原样返回第一次的回答，交给用户判断。
                Ok((next, _)) => {
                    let (next_calls, _) = parse_tool_calls(&next, &names);
                    if next_calls.is_empty() && looks_like_plan(&next) {
                        answer
                    } else {
                        next
                    }
                }
                Err(error) => {
                    agent2api_provider_sdk::log_warn!("[Today AI] 纠正重问失败：{error}");
                    answer
                }
            }
        } else {
            answer
        }
    };

    emit(&mut sink, &model, &answer, &tools).await
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
    let tools = tools_of(body);
    let protocol = tool_protocol(&tools);

    // 工具调用 id → 工具名：`role:"tool"` 的结果消息只带 id，要靠它反查名字。
    let mut call_names: HashMap<String, String> = HashMap::new();
    let turns: Vec<(String, String)> = messages
        .iter()
        .filter_map(|message| {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .trim()
                .to_string();
            let mut text = message_text(message).unwrap_or_default();
            // 原生工具对话的形状上游不认识：把 assistant.tool_calls 写成
            // 「调过什么」，下一轮模型还能接着推理，而不是凭空丢一段历史。
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                let mut rendered = Vec::new();
                for call in calls {
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .or_else(|| call.get("name").and_then(Value::as_str))
                        .unwrap_or("unknown_tool")
                        .to_string();
                    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    if !id.is_empty() {
                        call_names.insert(id.to_string(), name.clone());
                    }
                    let raw = call
                        .pointer("/function/arguments")
                        .or_else(|| call.get("arguments"));
                    let arguments = match raw {
                        Some(Value::String(value)) => value.clone(),
                        Some(value) => value.to_string(),
                        None => "{}".to_string(),
                    };
                    rendered.push(format!("{name}({arguments})"));
                }
                let suffix = format!("[I called: {}]", rendered.join(", "));
                text = if text.trim().is_empty() {
                    suffix
                } else {
                    format!("{text}\n\n{suffix}")
                };
            }
            if role == "tool" || role == "function" {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let name = call_names
                    .get(id)
                    .cloned()
                    .or_else(|| {
                        message
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "tool".to_string());
                // 工具结果动辄几万字，留开头足够模型判断下一步。
                text = format!("[result of {name}]\n{}", clip_chars(&text, 8_000));
            }
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
    // 协议块先占预算，剩下的才轮到历史 —— 工具定义被砍掉，模型就调不动工具。
    let budget = PROMPT_BUDGET_CHARS
        .saturating_sub(protocol.chars().count())
        .max(CURRENT_BUDGET_CHARS + 1_000);
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
        if used + cost > budget {
            dropped += 1;
            continue;
        }
        used += cost;
        picked.push((role.as_str(), piece));
    }
    picked.reverse();

    let mut prompt = String::new();
    if !protocol.is_empty() {
        prompt.push_str(&protocol);
        prompt.push('\n');
    }
    if picked.is_empty() {
        prompt.push_str(&current);
        prompt.push_str(state_hint(&last.0, !protocol.is_empty()));
        return Ok(prompt);
    }
    prompt.push_str("以下是本次对话的历史，仅作上下文：\n");
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
    prompt.push_str(state_hint(&last.0, !protocol.is_empty()));
    Ok(prompt)
}

/// 收尾处的「当前状态」提示。
///
/// 只在**最后一条是工具结果**时加：这是模型最容易退化成叙述的地方（「我先
/// 看一下 updateEnabled 的实现」），而它此刻唯一该做的是发下一个调用或者
/// 收尾。把提示放在整段提示词的最末尾，是因为紧邻生成点的约束最有效。
fn state_hint(last_role: &str, has_tools: bool) -> &'static str {
    if !has_tools || !matches!(last_role, "tool" | "function") {
        return "";
    }
    "\n\n[STATE] The message above is the result of a tool you just called, so the task is still in \
progress. Reply with either the next <tool_call> block (if you still need a file, a command, or a \
search) or the final answer for the user. Never reply with a plan, an intention, or a progress note \
such as \"I will now look at ...\"."
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

// ─── 工具桥接：文本协议 ─────────────────────────────────────

/// 一条工具定义（`body.tools[i].function`）。
struct ToolSpec {
    name: String,
    description: String,
    schema: Value,
}

/// 从请求体里取工具定义：兼容 `tools` 与旧式 `functions`，跳过无名项。
///
/// `tool_choice: "none"` 时不注入 —— 客户端明确说了这轮别调工具。
fn tools_of(body: &Value) -> Vec<ToolSpec> {
    if body.get("tool_choice").and_then(Value::as_str) == Some("none") {
        return Vec::new();
    }
    let raw: Vec<Value> = match body.get("tools").and_then(Value::as_array) {
        Some(items) => items.clone(),
        None => body
            .get("functions")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|function| json!({ "type": "function", "function": function }))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut tools = Vec::new();
    for item in raw {
        let function = item.get("function").unwrap_or(&item);
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        tools.push(ToolSpec {
            name,
            description: function
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            schema: function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" })),
        });
        if tools.len() >= MAX_TOOLS {
            break;
        }
    }
    tools
}

/// 工具调用协议块。
///
/// 用英文写：Cursor 自己的 system prompt 是英文，混排中英会让模型在
/// 「用哪种语言思考」上摇摆，进而影响 JSON 的稳定性（与 tool-bridge 同一口径）。
fn tool_protocol(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n===== TOOL CALLING PROTOCOL (highest priority) =====\n\
         You cannot call tools natively in this session. Tools are invoked through a text protocol.\n\
         When you need to use a tool, output ONLY one or more blocks in exactly this shape:\n\
         <tool_call>{\"name\":\"<tool_name>\",\"arguments\":{<json object matching the schema>}}</tool_call>\n\
         \n\
         Rules:\n\
         1. Always wrap the JSON in <tool_call></tool_call>. Emit raw JSON with no markdown fence.\n\
         2. \"arguments\" MUST be a JSON object (not a string), following the tool schema exactly.\n\
         3. Emit one <tool_call> block per tool call, in the order they should run.\n\
         4. You may write a short sentence before the blocks; never write anything inside them except the JSON.\n\
         5. When no tool is needed, answer normally and do NOT emit any <tool_call> block.\n\
         6. Never invent tool names. Only the tools listed below exist.\n\
         7. NEVER answer with only a sentence like \"I will look at the file\" — if you need a file or a command, emit the <tool_call> block immediately in the same reply.\n\
         \n\
         Wrong (the client stops here and nothing happens):\n\
         \"I will take a look at the current implementation of updateEnabled first.\"\n\
         Right (the client runs the tool and sends you the result):\n\
         <tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"/abs/path/File.java\"}}</tool_call>\n\
         \n\
         Available tools:\n",
    );
    for (index, tool) in tools.iter().enumerate() {
        let desc = clip_chars(
            &tool.description.split_whitespace().collect::<Vec<_>>().join(" "),
            TOOL_DESC_LIMIT,
        );
        let schema = serde_json::to_string(&compress_schema(&tool.schema, 0))
            .unwrap_or_else(|_| "{}".to_string());
        out.push_str(&format!(
            "- {}: {}\n  arguments schema: {}\n",
            tool.name, desc, schema
        ));
        if out.chars().count() > TOOL_PROTOCOL_MAX_CHARS {
            let rest = tools.len() - index - 1;
            if rest > 0 {
                // 只省 schema，不省名字：模型知道「有 edit_file」才可能去调它，
                // 名字整个消失等于这把工具在本轮不存在（Cursor 的工具清单很长，
                // 被截掉的往往正是改文件、跑命令这些关键工具）。
                let names: Vec<&str> = tools[index + 1..]
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect();
                out.push_str(&format!(
                    "({rest} more tools omitted: {} — still callable by name, with the same\n\
                     <tool_call> shape and an arguments object built from the tool's purpose)\n",
                    names.join(", ")
                ));
            }
            break;
        }
    }
    out.push_str("===== END TOOL CALLING PROTOCOL =====\n");
    out
}

/// 裁剪 JSON Schema：只留「模型必须知道才写得出参数」的部分。
///
/// Cursor 的工具 schema 里塞了大量 `description` 与 `additionalProperties`，
/// 对纯文本通道没有语义价值，却会把提示词顶到上游的拒答线。
fn compress_schema(schema: &Value, depth: usize) -> Value {
    let Some(object) = schema.as_object() else {
        return json!({ "type": "object" });
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("object");
    if depth >= TOOL_SCHEMA_DEPTH {
        return json!({ "type": kind });
    }
    let mut out = serde_json::Map::new();
    for key in ["type", "enum", "required", "const", "default"] {
        if let Some(value) = object.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(items)) = object.get(key) {
            let items: Vec<Value> = items
                .iter()
                .map(|item| compress_schema(item, depth + 1))
                .collect();
            out.insert(key.to_string(), Value::Array(items));
        }
    }
    if let Some(items) = object.get("items") {
        out.insert("items".to_string(), compress_schema(items, depth + 1));
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        let mut compressed = serde_json::Map::new();
        for (name, sub) in properties {
            let mut value = compress_schema(sub, depth + 1);
            if let Some(description) = sub.get("description").and_then(Value::as_str) {
                let text = description.trim();
                if !text.is_empty() {
                    if let Some(target) = value.as_object_mut() {
                        target.insert(
                            "description".to_string(),
                            Value::String(clip_chars(text, 120)),
                        );
                    }
                }
            }
            compressed.insert(name.clone(), value);
        }
        out.insert("properties".to_string(), Value::Object(compressed));
    }
    if !out.contains_key("type") {
        out.insert("type".to_string(), Value::String("object".to_string()));
    }
    Value::Object(out)
}

/// 解析出来的一次工具调用（`arguments` 已是合法 JSON 对象的字符串）。
struct ToolCall {
    name: String,
    arguments: String,
}

/// 从模型输出里还原工具调用，返回（调用列表，去掉调用块后的正文）。
///
/// 依次尝试：`<tool_call>` 块 → 整段 JSON → 文末最后一个平衡 JSON 对象。
/// 全部落空就当普通回答 —— 宁可少调一次，也不猜一个可能改坏文件的调用。
fn parse_tool_calls(answer: &str, allowed: &[String]) -> (Vec<ToolCall>, String) {
    if allowed.is_empty() {
        return (Vec::new(), answer.to_string());
    }
    let mut calls = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (start, end, inner) in tagged_blocks(answer) {
        let Some(json) = json_in(&inner) else { continue };
        let Ok(parsed) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let found = calls_of(&parsed, allowed);
        if found.is_empty() {
            continue;
        }
        calls.extend(found);
        ranges.push((start, end));
    }
    if !calls.is_empty() {
        let mut text = String::new();
        let mut cursor = 0;
        for (start, end) in ranges {
            text.push_str(&answer[cursor..start]);
            cursor = end;
        }
        text.push_str(&answer[cursor..]);
        return (calls, text.trim().to_string());
    }

    // 整段就是一个 JSON（模型偶尔省略 <tool_call> 标签）。
    let whole = answer.trim();
    if whole.starts_with('{') || whole.starts_with('[') {
        if let Ok(parsed) = serde_json::from_str::<Value>(whole) {
            let found = calls_of(&parsed, allowed);
            if !found.is_empty() {
                return (found, String::new());
            }
        }
    }

    // 前面写了一句「我来读一下这个文件」，后面跟一个裸 JSON。
    let mut cursor = whole.len();
    while let Some(position) = whole[..cursor].rfind('{') {
        if let Some(json) = balanced_json(whole, position) {
            if let Ok(parsed) = serde_json::from_str::<Value>(json) {
                let found = calls_of(&parsed, allowed);
                if !found.is_empty() {
                    return (found, whole[..position].trim().to_string());
                }
            }
        }
        cursor = position;
    }

    (Vec::new(), answer.to_string())
}

/// 扫出所有 `<tool_call>…</tool_call>`，返回（起, 止, 内容）。
fn tagged_blocks(text: &str) -> Vec<(usize, usize, String)> {
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find("<tool_call>") {
        let start = cursor + offset;
        let inner_start = start + "<tool_call>".len();
        let Some(close) = text[inner_start..].find("</tool_call>") else {
            break;
        };
        let inner_end = inner_start + close;
        let end = inner_end + "</tool_call>".len();
        blocks.push((
            start,
            end,
            text[inner_start..inner_end].trim().to_string(),
        ));
        cursor = end;
    }
    blocks
}

/// 从一段文本里取 JSON：本身以 `{` / `[` 开头就用整段，否则找第一个 `{`
/// 再做括号配对（跳过字符串里的括号与转义）。
fn json_in(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(trimmed);
    }
    let offset = text.find('{')?;
    balanced_json(text, offset)
}

/// 从 `start` 位置的 `{` 起找配对的 `}`。
///
/// 按字节遍历是安全的：结构字符都是 ASCII，多字节 UTF-8 的每个字节都
/// ≥ 0x80，不会被误判；返回的切片端点也都落在字符边界上。
fn balanced_json(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 把一段 JSON 值里的调用项取出来：裸对象、数组、`{tool_calls:[…]}` 都认。
fn calls_of(value: &Value, allowed: &[String]) -> Vec<ToolCall> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| normalise_call(item, allowed))
            .collect(),
        Value::Object(object) => match object.get("tool_calls") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| normalise_call(item, allowed))
                .collect(),
            _ => normalise_call(value, allowed).into_iter().collect(),
        },
        _ => Vec::new(),
    }
}

/// 校验并规范化一次调用：名字必须在本次请求声明的工具里，参数必须是
/// 合法 JSON 对象（模型有时把 arguments 写成字符串）。
fn normalise_call(value: &Value, allowed: &[String]) -> Option<ToolCall> {
    let function = value.get("function").unwrap_or(value);
    let name = function
        .get("name")
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if name.is_empty() || !allowed.iter().any(|item| item == &name) {
        return None;
    }
    let raw = function
        .get("arguments")
        .or_else(|| function.get("params"))
        .or_else(|| function.get("parameters"));
    let arguments = match raw {
        None => "{}".to_string(),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                "{}".to_string()
            } else {
                serde_json::from_str::<Value>(trimmed).ok()?.to_string()
            }
        }
        Some(value) => value.to_string(),
    };
    Some(ToolCall { name, arguments })
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

/// 长轮询 `message-changes`，直到助理那条消息稳定下来。返回（正文，最后序号）。
///
/// ── 为什么收到回答还要再探一次 ───────────────────────────────
/// v3 的变更项是**整条消息快照**（不是增量），但协议没有 `isFinal` 之类的
/// 完成位。收到一条助理消息后继续用短 `waitMs` 探：没有新变更就说明这条
/// 回答已经写完；有新变更（同一 `id`、更长的正文）就用新的。
///
/// ── 为什么是「连续两次静默」而不是「一次」 ────────────────────
/// 一次静默只能证明这三秒里没有新变更，不能证明模型写完了。模型先吐一句
/// 「我先看一下」，再去生成 `<tool_call>` 的 JSON 时，空档经常超过一次
/// 静默窗口；按一次就收工，调用块会被整段丢掉，客户端表现为任务突然结束。
/// 另外，正文里还有没闭合的 `<tool_call>` 时也继续等 —— 半个标签明显不是
/// 最终回答。
///
/// 返回最后一条消息的 `sequence`：追问时从它之后开始读，免得把上一轮的
/// 回答当成本轮结果。
async fn await_answer(
    ctx: &PluginCtx,
    http_client: &reqwest::Client,
    token: &mut String,
    account: &str,
    conversation: &str,
) -> Result<(String, i64), String> {
    await_answer_after(ctx, http_client, token, account, conversation, 0).await
}

/// 从 `start_after` 之后读变更（追问时用它跳过已经读过的那条回答）。
async fn await_answer_after(
    ctx: &PluginCtx,
    http_client: &reqwest::Client,
    token: &mut String,
    account: &str,
    conversation: &str,
    start_after: i64,
) -> Result<(String, i64), String> {
    if !conversation
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err("上游返回的 conversationId 含有无法拼进 URL 的字符".to_string());
    }
    let base = endpoints::api_base_url();
    let started = http::now_ms();
    let mut after = start_after;
    let mut answer = String::new();
    let mut quiet = 0_usize;
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
        if answer.is_empty() {
            continue;
        }
        if updated {
            quiet = 0;
            continue;
        }
        quiet += 1;
        // 未闭合的 `<tool_call>` 说明调用块还在写，多给几次静默窗口；但也不能
        // 真的无限等 —— 模型把标签写坏的场合要能收工，而不是拖到超时报错。
        let grace = if has_open_tool_block(&answer) {
            OPEN_BLOCK_GRACE_PROBES
        } else {
            SETTLE_QUIET_PROBES
        };
        if quiet < grace {
            continue;
        }
        // 上游的「软拒绝」是 HTTP 200 + 一句固定话术，必须在这里拦住：
        // 原样返回等于把拒答伪装成回答，客户端和用户都无从判断。
        if let Some(reason) = refusal_of(&answer) {
            return Err(reason);
        }
        return Ok((answer, after));
    }
}

/// 正文里是否有还没闭合的 `<tool_call>`。
fn has_open_tool_block(text: &str) -> bool {
    match (text.rfind("<tool_call>"), text.rfind("</tool_call>")) {
        (Some(open), Some(close)) => open > close,
        (Some(_), None) => true,
        _ => false,
    }
}

/// 取一段预览用于日志：折掉换行、截到 `limit` 个字符。
///
/// 只用于诊断行，别把整段代码塞进日志 —— 网关的日志页有容量上限。
fn preview_of(text: &str, limit: usize) -> String {
    let flat: String = text
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect();
    clip_chars(flat.trim(), limit)
}

/// 这条回答是不是「只说了要做什么，没给调用块」。
///
/// 命中后追加 [`NUDGE_PROMPT`] 重问一轮。判定放得很保守：必须带工具、
/// 必须短（长回复里的「我先」通常只是叙述步骤），必须命中中文/英文的
/// 计划句式之一。
fn looks_like_plan(answer: &str) -> bool {
    let trimmed = answer.trim();
    if trimmed.is_empty() || trimmed.chars().count() > PLAN_MAX_CHARS {
        return false;
    }
    // 已经给了调用块就不是空转。
    if trimmed.contains("<tool_call>") {
        return false;
    }
    let lowered = trimmed.to_lowercase();
    PLAN_MARKERS
        .iter()
        .any(|marker| lowered.contains(&marker.to_lowercase()))
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

/// 推 head + 若干 chunk + `[DONE]`。
///
/// 没有工具调用时是「role → 正文 → stop」三帧；解析到工具调用时改成
/// 「role(+正文) → tool_calls → finish_reason:"tool_calls"」—— 客户端
/// 据此继续下一轮 Agent 循环，而不是把一句「我先看一下」当成最终回答。
///
/// 非流式客户端由网关自己聚合（`aggregate_frame_stream`），所以插件只写
/// 这一条路径 —— 两条路径的正文迟早会分叉。
async fn emit(
    sink: &mut ConversationSink<'_>,
    model: &str,
    answer: &str,
    tools: &[ToolSpec],
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
    let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    let (calls, text) = parse_tool_calls(answer, &names);
    // 诊断：上游到底给了什么。分不清「模型只说了一句」还是「调用块被截掉」时，
    // 这一行是唯一的一锤定音 —— 上游原文里有 <tool_call> 就是解析/时机问题，
    // 没有就是模型遵从度问题（两种的修法完全不同）。
    agent2api_provider_sdk::log_info!(
        "[Today AI] 上游原文 {} 字 | 含 <tool_call> 标签: {} | 解析出 {} 个调用 | 预览: {}",
        answer.chars().count(),
        answer.contains("<tool_call>"),
        calls.len(),
        preview_of(answer, 120)
    );

    if calls.is_empty() {
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
        return Ok(());
    }

    // 正文（模型常在调用前说一句「我来读一下这个文件」）先发，再一次性给全部调用。
    let mut first = serde_json::Map::new();
    first.insert("role".to_string(), Value::String("assistant".to_string()));
    if !text.is_empty() {
        first.insert("content".to_string(), Value::String(text));
    }
    let payload: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            json!({
                "index": index,
                "id": format!("call_{}", unique_id()),
                "type": "function",
                "function": {
                    "name": call.name.clone(),
                    "arguments": call.arguments.clone(),
                },
            })
        })
        .collect();
    let frames = [
        json!({
            "id": id.clone(),
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_out,
            "choices": [{ "index": 0, "delta": Value::Object(first), "finish_reason": null }],
        }),
        json!({
            "id": id.clone(),
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_out,
            "choices": [{ "index": 0, "delta": { "tool_calls": payload }, "finish_reason": null }],
        }),
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_out,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
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

    #[test]
    fn injects_tool_protocol() {
        let body = serde_json::json!({
            "messages": [{ "role": "user", "content": "读一下 src/main.rs" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file from disk",
                    "parameters": {
                        "type": "object",
                        "properties": { "path": { "type": "string", "description": "Absolute path" } },
                        "required": ["path"]
                    }
                }
            }]
        });
        let prompt = prompt_of(&body, Plan::Full).expect("prompt");
        assert!(prompt.contains("TOOL CALLING PROTOCOL"));
        assert!(prompt.contains("read_file"));
        assert!(prompt.contains("\"path\""));
        assert!(prompt.contains("读一下 src/main.rs"));
    }

    #[test]
    fn no_protocol_without_tools() {
        let body = serde_json::json!({ "messages": [{ "role": "user", "content": "你好" }] });
        let prompt = prompt_of(&body, Plan::Full).expect("prompt");
        assert!(!prompt.contains("TOOL CALLING PROTOCOL"));
    }

    #[test]
    fn tool_choice_none_skips_protocol() {
        let body = serde_json::json!({
            "tool_choice": "none",
            "messages": [{ "role": "user", "content": "你好" }],
            "tools": [{ "type": "function", "function": { "name": "read_file" } }]
        });
        let prompt = prompt_of(&body, Plan::Full).expect("prompt");
        assert!(!prompt.contains("TOOL CALLING PROTOCOL"));
    }

    #[test]
    fn flattens_tool_history() {
        let body = serde_json::json!({
            "messages": [
                { "role": "user", "content": "看一下 main.rs" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "fn main() {}" },
                { "role": "assistant", "content": "文件很小。" },
                { "role": "user", "content": "继续" }
            ]
        });
        let prompt = prompt_of(&body, Plan::Full).expect("prompt");
        assert!(prompt.contains("[I called: read_file({\"path\":\"src/main.rs\"})]"));
        assert!(prompt.contains("[result of read_file]"));
        assert!(prompt.contains("fn main() {}"));
    }

    #[test]
    fn parses_tagged_calls() {
        let allowed = vec!["read_file".to_string(), "run".to_string()];
        let answer = r#"我先读一下。
<tool_call>{"name":"read_file","arguments":{"path":"a.rs"}}</tool_call>
<tool_call>{"name":"run","arguments":"{\"cmd\":\"ls\"}"}</tool_call>"#;
        let (calls, text) = parse_tool_calls(answer, &allowed);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(calls[1].arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(text, "我先读一下。");
    }

    #[test]
    fn drops_unknown_and_broken_calls() {
        let allowed = vec!["read_file".to_string()];
        let answer = "<tool_call>{\"name\":\"delete_everything\",\"arguments\":{}}</tool_call>";
        let (calls, text) = parse_tool_calls(answer, &allowed);
        assert!(calls.is_empty(), "未声明的工具必须丢弃");
        assert_eq!(text, answer, "丢弃后按普通文本返回");

        let broken = "<tool_call>{not json}</tool_call>";
        let (calls, _) = parse_tool_calls(broken, &allowed);
        assert!(calls.is_empty());
    }

    #[test]
    fn parses_bare_json_call() {
        let allowed = vec!["read_file".to_string()];
        let answer = "我来读一下。{\"name\":\"read_file\",\"arguments\":{\"path\":\"b.rs\"}}";
        let (calls, text) = parse_tool_calls(answer, &allowed);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(text, "我来读一下。");
    }

    #[test]
    fn protocol_budget_is_bounded() {
        let mut tools = Vec::new();
        for index in 0..MAX_TOOLS {
            tools.push(ToolSpec {
                name: format!("tool_{index}"),
                description: "描述".repeat(500),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": { "x": { "type": "string" } }
                }),
            });
        }
        let protocol = tool_protocol(&tools);
        assert!(protocol.chars().count() <= TOOL_PROTOCOL_MAX_CHARS + 2_000);
        assert!(protocol.contains("more tools omitted") || tools.len() < MAX_TOOLS);
    }

    /// 最后一条是工具结果时必须补状态提示：这是模型最容易退化成叙述的位置。
    #[test]
    fn state_hint_follows_tool_result() {
        let body = serde_json::json!({
            "messages": [
                { "role": "user", "content": "把开关逻辑改一下" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"A.java\"}" }
                    }]
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "class A {}" }
            ],
            "tools": [{ "type": "function", "function": { "name": "read_file" } }]
        });
        let prompt = prompt_of(&body, Plan::Full).expect("prompt");
        assert!(prompt.contains("[STATE] The message above is the result of a tool"));
        assert!(
            prompt.trim_end().ends_with("progress note such as \"I will now look at ...\"."),
            "状态提示要落在整段提示词的最末尾（紧邻生成点）"
        );

        // 最后一条是用户提问时不需要（那是在问新问题，不是续做）。
        let fresh = serde_json::json!({
            "messages": [{ "role": "user", "content": "你好" }],
            "tools": [{ "type": "function", "function": { "name": "read_file" } }]
        });
        let prompt = prompt_of(&fresh, Plan::Full).expect("prompt");
        assert!(!prompt.contains("[STATE]"));
    }

    /// 没声明工具时不该出现状态提示（提示里提到 `<tool_call>` 会误导模型）。
    #[test]
    fn state_hint_needs_tools() {
        assert!(state_hint("tool", false).is_empty());
        assert!(!state_hint("tool", true).is_empty());
        assert!(state_hint("user", true).is_empty());
    }

    #[test]
    fn detects_plan_without_action() {
        assert!(looks_like_plan(
            "我先看一下当前 `updateEnabled` 和 `saveConfig` 的完整实现，再动手改。"
        ));
        assert!(looks_like_plan("I will look at the service implementation first."));
        // 给了调用块就不是空转。
        assert!(!looks_like_plan(
            "我先读一下。<tool_call>{\"name\":\"read_file\",\"arguments\":{}}</tool_call>"
        ));
        // 长回复里的「我先」通常只是叙述，别去追问。
        let long = format!("我先看一下实现，然后解释。{}", "细节说明。".repeat(200));
        assert!(!looks_like_plan(&long));
        // 正常收尾。
        assert!(!looks_like_plan("改完了，共改动两处。"));
    }

    #[test]
    fn detects_open_tool_block() {
        assert!(has_open_tool_block("<tool_call>{\"name\":\"read_file\""));
        assert!(!has_open_tool_block(
            "<tool_call>{\"name\":\"read_file\"}</tool_call>"
        ));
        assert!(!has_open_tool_block("普通回答，没有标签"));
        assert!(has_open_tool_block(
            "<tool_call>{\"a\":1}</tool_call> 然后 <tool_call>{\"b\":2"
        ));
    }
}
