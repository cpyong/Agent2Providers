//! Today AI 模型目录：`GET {网关}/v1/llm/models` + 进程内缓存。
//!
//! ── 上游形态（实测，2026-10）──────────────────────────────────
//! 网关 `https://api.todayai.cn` 只暴露 **`/v1/llm/models`**（OpenAI 的
//! `/v1/models` 是 404），鉴权是 `Authorization: Bearer <jwt>`，令牌由账号
//! 接口换发（见 `credentials.rs`）。响应是 `{models:[{modelId}, …]}`，
//! 没有名称 / 能力位 / 上下文长度 —— 缺的字段按保守口径补（工具位与思考位
//! 默认关：本家的转发通道是助理会话，给不出 `tool_calls`）。解析同时容忍
//! `{data:[{id, …}]}` 与裸数组，免得上游哪天换成 OpenAI 形态就整份目录失效。
//!
//! ── 拉不到时不编模型名 ──────────────────────────────────────
//! 与 Loomy 同一纪律：没有内置兜底清单（`meta().default_models` 为空），
//! 拉取失败就如实报失败并保留旧清单 —— 编一个模型名会让客户端选到一个
//! 上游根本不存在的模型。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 只有进程内存（自动路径 10 分钟 TTL；用户手动「刷新模型清单」时 `force`
//! 绕过）。**不落盘** —— 网关自己会缓存插件返回的清单，插件再存一份就是
//! 两套状态互相不一致。

use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::client;
use super::endpoints;
use crate::http;

/// 自动路径的缓存 TTL
const TTL_MS: i64 = 10 * 60 * 1000;

/// 模型列表路径（相对网关基址）
pub const MODELS_PATH: &str = "/v1/llm/models";

/// 进程内缓存
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

/// 取状态句柄（毒锁按「继续用」处理：这份缓存是可重建的展示数据）
fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// 在锁内执行一段取值
fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 从上游 `capabilities` 对象里取布尔位（缺失 / 非布尔一律 false）
fn caps_bool(caps: Option<&serde_json::Map<String, Value>>, key: &str) -> bool {
    caps.and_then(|caps| caps.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `capabilities.input_modalities` 数组里有没有指定模态（大小写不敏感）
fn caps_modal(caps: Option<&serde_json::Map<String, Value>>, modal: &str) -> bool {
    caps.and_then(|caps| caps.get("input_modalities"))
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|value| value.as_str().is_some_and(|text| text.eq_ignore_ascii_case(modal)))
        })
}

/// 把一个目录条目映射成聚合层认的形态
/// （键名对照网关的 `core/models/shape.rs::list_item`）。
///
/// 本家的条目只有一个 `modelId`；`id` / `name` / `capabilities` 是给
/// 「上游哪天换成 OpenAI 形态」留的兼容读法。
fn map_entry(row: &Value) -> Option<Value> {
    let id = ["id", "modelId"]
        .iter()
        .find_map(|key| row.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let name = row
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    let caps = row.get("capabilities").and_then(Value::as_object);
    let mut item = json!({
        "id": id,
        "name": name,
        // 目录只给 `modelId`，没有任何能力位。默认按**保守口径**：工具位
        // 与思考位都关 —— 本家的转发通道是助理会话，回答是成品文本，没有
        // `tool_calls`，也没有独立的思考流（见 `chat.rs` 的模块头）。申报成
        // 「支持」会让客户端发来工具定义却永远等不到调用。
        "supportsToolCall": caps_bool(caps, "function_calling"),
        "supportsReasoning": caps_bool(caps, "reasoning"),
    });
    if let Some(object) = item.as_object_mut() {
        if caps_bool(caps, "vision") || caps_modal(caps, "image") {
            object.insert("supportsImages".to_string(), Value::Bool(true));
        }
        if caps_modal(caps, "video") {
            object.insert("supportsVideo".to_string(), Value::Bool(true));
        }
        // 上游给了上下文 / 输出上限就透传
        let pick_number = |keys: &[&str]| -> Option<i64> {
            for key in keys {
                if let Some(value) = row.get(*key).and_then(Value::as_i64) {
                    if value > 0 {
                        return Some(value);
                    }
                }
            }
            None
        };
        if let Some(max_input) = pick_number(&[
            "context_length",
            "context_window",
            "input_token_limit",
            "max_input_tokens",
            "maxInputTokens",
        ]) {
            object.insert("maxInputTokens".to_string(), Value::from(max_input));
        }
        if let Some(max_output) = pick_number(&[
            "max_output_tokens",
            "max_completion_tokens",
            "max_tokens",
            "maxOutputTokens",
        ]) {
            object.insert("maxOutputTokens".to_string(), Value::from(max_output));
        }
    }
    Some(item)
}

/// 解析目录响应：本家 `{models:[{modelId}]}`，兼容 OpenAI `{data:[...]}` 与裸数组
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| payload.get("data").and_then(Value::as_array))
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(map_entry).collect()
}

/// 当前清单 + 元数据
pub fn catalog() -> proto::ModelList {
    with_state(|slot| match slot.as_ref() {
        Some(state) => proto::ModelList {
            models: state.models.clone(),
            meta: proto::CatalogMeta {
                // 本家没有内置兜底清单：有内容就等于远程拉到过
                remote: !state.models.is_empty(),
                refreshed_at: state.fetched_at,
            },
        },
        None => proto::ModelList::default(),
    })
}

/// 拉一次模型目录并落地（内存缓存）。
pub async fn refresh(
    client: &reqwest::Client,
    access_token: &str,
    force: bool,
) -> proto::RefreshOutcome {
    let now = http::now_ms();
    if !force {
        let fresh = with_state(|slot| {
            slot.as_ref()
                .map(|state| !state.models.is_empty() && now - state.fetched_at < TTL_MS)
                .unwrap_or(false)
        });
        if fresh {
            return proto::RefreshOutcome::unchanged();
        }
    }
    let mut headers = client::api_headers();
    headers.push((
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    ));
    let url = format!("{}{}", endpoints::api_base_url(), MODELS_PATH);
    let (status, payload) = match crate::http::get_json(client, &url, &headers).await {
        Ok(result) => result,
        Err(error) => return proto::RefreshOutcome::failed(error),
    };
    if !(200..300).contains(&status) {
        return proto::RefreshOutcome::failed(client::describe_error(
            status,
            &payload,
            "模型目录查询",
        ));
    }
    let models = parse_models(&payload);
    if models.is_empty() {
        return proto::RefreshOutcome::failed("Today AI 模型目录为空或格式不认识（保留现有清单）");
    }
    let count = models.len();
    with_state(|slot| {
        *slot = Some(CatalogState {
            models: models.clone(),
            fetched_at: now,
        });
    });
    proto::RefreshOutcome::refreshed(count)
}
