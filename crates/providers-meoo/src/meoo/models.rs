//! 模型目录：网关 `/models` 在线清单 + 内置兜底，10 分钟进程内缓存。
//!
//! ── 为什么不编模型名 ────────────────────────────────────────
//! 兜底清单逐条抄自公开的 Meoo 官方网关集成（dsh-meoo 的
//! `FALLBACK_MODELS`），是真实用过的 model id；官方积分文档里那些展示名
//! （Qwen4-Max-Preview 之类）没有给出接口 id，一律不写进目录。
//! 拉到在线目录就以在线为准，并在 `meta.remote` 里如实标出来源。

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::client;
use crate::http;

const TTL_MS: i64 = 10 * 60 * 1_000;

/// `(id, name)`：Meoo AI 网关公开集成里实际用过的 9 个 id。
const FALLBACK: &[(&str, &str)] = &[
    ("qwen3.8-flash", "Qwen3.8-Flash"),
    ("qwen3.8-max", "Qwen3.8-Max"),
    ("qwen3.7-max", "Qwen3.7-Max"),
    ("qwen3.7-plus", "Qwen3.7-Plus"),
    ("deepseek-v4.1-flash-1017", "DeepSeek V4.1 Flash"),
    ("deepseek-v4-pro-0813", "DeepSeek V4 Pro"),
    ("deepseek-v4-flash-0731", "DeepSeek V4 Flash"),
    ("glm-5.2-fast-preview", "GLM-5.2-Fast"),
    ("glm-5.2", "GLM-5.2"),
];

struct Cached {
    models: Vec<Value>,
    fetched_at: i64,
}

fn state() -> &'static Mutex<Option<Cached>> {
    static STATE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

fn with_state<R>(f: impl FnOnce(&mut Option<Cached>) -> R) -> R {
    let mut guard = state().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

fn record(id: &str, name: &str, remote: bool) -> Value {
    json!({
        "id": id,
        "name": name,
        "supportsToolCall": true,
        "supportsReasoning": false,
        "supportsImages": false,
        "maxInputTokens": 200_000,
        "maxOutputTokens": 64_000,
        "upstreamModel": id,
        "source": if remote { "gateway" } else { "fallback" },
    })
}

pub fn snapshot() -> Vec<Value> {
    FALLBACK
        .iter()
        .map(|(id, name)| record(id, name, false))
        .collect()
}

/// 把上游条目归一成模型记录（字符串 / 对象 / 多种 id 键都认）。
pub fn map_entry(entry: &Value, remote: bool) -> Option<Value> {
    if let Value::String(id) = entry {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let name = FALLBACK
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, name)| *name)
            .unwrap_or(id);
        return Some(record(id, name, remote));
    }
    let id = super::credentials::pick(entry, &["id", "model_id", "modelId", "name"]);
    if id.is_empty() {
        return None;
    }
    let name = super::credentials::pick(entry, &["name", "display_name", "displayName"]);
    let label = if name.is_empty() {
        FALLBACK
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, label)| *label)
            .unwrap_or(id.as_str())
    } else {
        name.as_str()
    };
    let mut row = record(&id, label, remote);
    if let Some(context) = entry.get("context_window").or_else(|| entry.get("max_context_tokens")) {
        row["maxInputTokens"] = context.clone();
    }
    if let Some(max) = entry.get("max_tokens").or_else(|| entry.get("max_output_tokens")) {
        row["maxOutputTokens"] = max.clone();
    }
    Some(row)
}

pub fn parse_catalog(list: &[Value]) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for entry in list {
        let Some(row) = map_entry(entry, true) else {
            continue;
        };
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(id.to_string()) {
            models.push(row);
        }
    }
    models
}

pub fn catalog() -> proto::ModelList {
    with_state(|slot| match slot {
        Some(cached) => proto::ModelList {
            models: cached.models.clone(),
            meta: proto::CatalogMeta {
                remote: true,
                refreshed_at: cached.fetched_at,
            },
        },
        None => proto::ModelList {
            models: snapshot(),
            meta: proto::CatalogMeta::default(),
        },
    })
}

/// `force = false` 且缓存未过期 → `unchanged()`（界面不该显示「刷新失败」）。
pub async fn refresh(
    ctx: &agent2api_provider_sdk::PluginCtx,
    account: &str,
    force: bool,
) -> proto::RefreshOutcome {
    let fresh = with_state(|slot| {
        slot.as_ref()
            .filter(|cached| !force && http::now_ms() - cached.fetched_at < TTL_MS)
            .is_some()
    });
    if fresh {
        return proto::RefreshOutcome::unchanged();
    }
    let credential = match super::credentials::load(ctx, account).await {
        Ok(credential) => credential,
        Err(error) => return proto::RefreshOutcome::failed(error),
    };
    let list = match client::gateway_models(ctx, &credential).await {
        Ok(list) => list,
        Err(error) => return proto::RefreshOutcome::failed(error),
    };
    let models = parse_catalog(&list);
    if models.is_empty() {
        return proto::RefreshOutcome::failed("Meoo 网关没有返回可用模型，保留内置清单");
    }
    let count = models.len();
    with_state(|slot| {
        *slot = Some(Cached {
            models,
            fetched_at: http::now_ms(),
        });
    });
    proto::RefreshOutcome::refreshed(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_is_not_empty_and_marked_fallback() {
        let list = snapshot();
        assert_eq!(list.len(), 9);
        assert_eq!(list[0]["id"], "qwen3.8-flash");
        assert_eq!(list[0]["source"], "fallback");
    }

    #[test]
    fn parses_string_and_object_entries() {
        let rows = parse_catalog(&[
            json!("qwen3.8-flash"),
            json!({"id": "new-model", "name": "新模型", "context_window": 32000}),
            json!({"modelId": "fallback-id"}),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["name"], "Qwen3.8-Flash");
        assert_eq!(rows[1]["maxInputTokens"], 32000);
        assert_eq!(rows[2]["source"], "gateway");
    }

    #[test]
    fn deduplicates_and_drops_empty() {
        let rows = parse_catalog(&[json!({"id": "a"}), json!({"id": "a"}), json!({}), json!(null)]);
        assert_eq!(rows.len(), 1);
    }
}
