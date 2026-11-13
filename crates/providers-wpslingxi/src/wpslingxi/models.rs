//! 模型目录：`GET /api/aioffice/v1/sessions/plans` + 进程内缓存。
//!
//! ── 上游形状 ────────────────────────────────────────────────
//! `data.plans[]` 是「模式」（`plan_pro` / `plan_max`），`data.models[]` 是
//! 真正的模型；两者条目同形：`key`、`name`、`max_context_tokens`、
//! `max_new_tokens`、`support_multimodal`、`reasoning_efforts[]`、
//! `multiplier`（倍率，可能是 `0.12x` / `0.75倍` 这样的文本）。目录按同一
//! 份 Cookie 拉取，两个域都试（见 `client::call`）。
//!
//! ── 内置快照不是编的模型名 ──────────────────────────────────
//! [`snapshot`] 是上游桌面客户端 1.3.31 内置的 14 个下拉项（公开实现
//! `Coding2API/src/provider/lingxi/client.py` 的 `DEFAULT_MODELS` 逐项照抄）。
//! 上游目录不可达时用它兜底，`CatalogMeta.remote` 会如实标成 `false`，
//! 界面显示「内置清单」。**不**为了好看补上游没有的模型。
//!
//! ── 缓存 ───────────────────────────────────────────────────
//! 只有进程内存（自动路径 10 分钟 TTL；用户手动刷新时 `force` 绕过）。不落盘：
//! 网关自己缓存插件返回的清单，插件再存一份就是两套状态互相不一致。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::{client, credentials::Credential, endpoints};
use crate::http;

const TTL_MS: i64 = 10 * 60 * 1000;

struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

fn state() -> &'static Mutex<Option<CatalogState>> {
    static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// 在锁内执行一段取值（毒锁按「继续用」处理：这份缓存是可重建的展示数据）。
fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mut guard = state().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 上游客户端 1.3.31 的内置目录（14 项），只在远程目录拉不到时兜底。
///
/// 顺序与倍率按公开实现照抄；`supportsToolCall` 恒 true 是因为桌面端
/// agent-runtime 通路原生给 `tool_calls`。
pub fn snapshot() -> Vec<Value> {
    const ROWS: &[(&str, &str, f64, i64, bool, bool)] = &[
        ("auto", "自动推荐 (Auto)", 1.0, 1_048_576, false, true),
        ("deepseek-v4.1-flash", "DeepSeek V4.1 Flash", 0.12, 1_048_576, true, true),
        ("deepseek-v4-flash-0731", "DeepSeek V4 Flash", 0.09, 1_048_576, false, true),
        ("deepseek-v4-pro-0813", "DeepSeek V4 Pro", 0.40, 1_048_576, false, true),
        ("kimi-k3", "Kimi-K3", 1.60, 1_048_576, true, true),
        ("GLM-5.3", "GLM-5.3", 0.75, 1_048_576, false, true),
        ("GLM-5.3-Flash", "GLM-5.3-Flash", 0.11, 1_048_576, true, true),
        ("qwen3.8_max", "Qwen3.8-Max", 0.80, 1_048_576, true, true),
        ("qwen3.8_flash", "Qwen3.8-Flash", 0.08, 1_048_576, true, true),
        ("minimax_m3", "MiniMax-M3", 0.20, 1_048_576, true, false),
        (
            "doubao_seed_2_1_pro_260628",
            "Doubao-Seed-2.1 Pro",
            0.50,
            262_144,
            true,
            true,
        ),
        ("mimo-v2.6-pro", "Xiaomi MiMo-V2.6 Pro", 0.10, 1_048_576, true, false),
        ("plan_pro", "Pro 模式 (深度推理)", 1.0, 1_048_576, false, true),
        ("plan_max", "Max 模式 (复杂任务)", 1.0, 1_048_576, false, true),
    ];
    ROWS
        .iter()
        .map(|(id, name, rate, context, vision, reasoning)| {
            json!({
                "id": *id,
                "name": *name,
                "supportsToolCall": true,
                "supportsReasoning": *reasoning,
                "supportsImages": *vision,
                "maxInputTokens": *context,
                "maxOutputTokens": 65_536,
                "creditRate": *rate,
                "credits": format!("{rate}x"),
            })
        })
        .collect()
}

/// 倍率文本 → 数字。只认「一个数字 + 可选倍率标记」，其余一律不给倍率：
/// 把 `模式` 这类文案错认成数字比不显示倍率糟得多。
fn multiplier(value: Option<&Value>) -> Option<f64> {
    let raw = match value? {
        Value::Number(number) => return number.as_f64(),
        Value::String(text) => text.trim().to_string(),
        _ => return None,
    };
    let trimmed = raw
        .trim_start_matches(|ch: char| "~～≈约×xX ".contains(ch))
        .trim_end_matches(|ch: char| "×xX倍率 ".contains(ch))
        .trim();
    trimmed.parse::<f64>().ok().filter(|number| number.is_finite())
}

fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64().filter(|number| *number > 0),
        Value::String(text) => text.trim().parse::<i64>().ok().filter(|number| *number > 0),
        _ => None,
    }
}

/// 一个目录条目 → 聚合层认的形态（键名对照网关 `models::list_item`）。
fn map_entry(row: &Value) -> Option<Value> {
    let id = ["key", "id", "modelId"]
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
    let mut item = json!({
        "id": id,
        "name": name,
        // agent-runtime 通路原生给 tool_calls；缺字段时也不虚构一个 false
        "supportsToolCall": true,
        "supportsReasoning": row
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map(|efforts| !efforts.is_empty())
            .unwrap_or(false),
        "supportsImages": row
            .get("support_multimodal")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    });
    if let Some(object) = item.as_object_mut() {
        if let Some(max_input) = integer(row.get("max_context_tokens")) {
            object.insert("maxInputTokens".to_string(), Value::from(max_input));
        }
        if let Some(max_output) = integer(row.get("max_new_tokens")) {
            object.insert("maxOutputTokens".to_string(), Value::from(max_output));
        }
        if let Some(rate) = multiplier(row.get("multiplier")) {
            object.insert("creditRate".to_string(), json!(rate));
            object.insert("credits".to_string(), json!(format!("{rate}x")));
        }
    }
    Some(item)
}

/// 解析目录响应：`data.plans[]` + `data.models[]`，兼容 `{data:[…]}` 与裸数组。
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let data = payload.get("data").unwrap_or(payload);
    let mut rows: Vec<Value> = Vec::new();
    for key in ["plans", "models"] {
        if let Some(items) = data.get(key).and_then(Value::as_array) {
            rows.extend(items.iter().cloned());
        }
    }
    if rows.is_empty() {
        if let Some(items) = data.as_array() {
            rows.extend(items.iter().cloned());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    rows.iter()
        .filter_map(map_entry)
        .filter(|item| {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            seen.insert(id.to_string())
        })
        .collect()
}

/// 当前清单 + 元数据。没拉过远程目录时给内置快照，并如实标 `remote: false`。
pub fn catalog() -> proto::ModelList {
    with_state(|slot| match slot.as_ref() {
        Some(state) => proto::ModelList {
            models: state.models.clone(),
            meta: proto::CatalogMeta {
                remote: true,
                refreshed_at: state.fetched_at,
            },
        },
        None => proto::ModelList {
            models: snapshot(),
            meta: proto::CatalogMeta::default(),
        },
    })
}

/// 拉一次远程目录并落地（内存缓存）。
pub async fn refresh(
    http_client: &reqwest::Client,
    credential: &Credential,
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
    let (_, payload) = match client::call(
        http_client,
        credential,
        reqwest::Method::GET,
        endpoints::EP_PLANS,
        None,
        Duration::from_secs(20),
    )
    .await
    {
        Ok(result) => result,
        Err(error) if error.starts_with(client::AUTH_ERROR_PREFIX) => {
            return proto::RefreshOutcome::failed(format!(
                "WPS 灵犀登录态已失效，请重新粘贴 wps_sid Cookie 后再刷新模型清单（{error}）"
            ));
        }
        Err(error) => return proto::RefreshOutcome::failed(error),
    };
    let models = parse_models(&payload);
    if models.is_empty() {
        return proto::RefreshOutcome::failed(
            "WPS 灵犀模型目录为空或格式不认识（保留现有清单，不编造模型名）",
        );
    }
    let count = models.len();
    with_state(|slot| {
        *slot = Some(CatalogState {
            models,
            fetched_at: now,
        });
    });
    proto::RefreshOutcome::refreshed(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_the_verified_upstream_client_list() {
        let models = snapshot();
        assert_eq!(models.len(), 14);
        assert_eq!(models[0]["id"], "auto");
        assert!(models.iter().any(|item| item["id"] == "qwen3.8_flash"));
        // 没拉过远程目录时给内置快照，并如实标成非远程
        assert_eq!(catalog().models.len(), 14);
        assert!(!catalog().meta.remote);
    }

    #[test]
    fn parse_plans_and_models_with_text_multiplier() {
        let payload = json!({
            "data": {
                "plans": [{ "key": "plan_pro", "name": "Pro 模式", "multiplier": "1.0x" }],
                "models": [{
                    "key": "GLM-5.3",
                    "name": "GLM-5.3",
                    "multiplier": "0.75倍",
                    "max_context_tokens": 1_048_576,
                    "max_new_tokens": 65_536,
                    "support_multimodal": true,
                    "reasoning_efforts": ["low", "high"],
                }],
            },
        });
        let models = parse_models(&payload);
        assert_eq!(models.len(), 2);
        let glm = &models[1];
        assert_eq!(glm["id"], "GLM-5.3");
        assert_eq!(glm["creditRate"], 0.75);
        assert_eq!(glm["maxInputTokens"], 1_048_576);
        assert_eq!(glm["supportsReasoning"], true);
        assert_eq!(glm["supportsImages"], true);
    }

    #[test]
    fn duplicate_keys_are_deduplicated_and_bad_rates_are_dropped() {
        let payload = json!({
            "data": {
                "plans": [{ "key": "auto", "name": "自动", "multiplier": "模式" }],
                "models": [{ "key": "auto", "name": "自动推荐" }],
            },
        });
        let models = parse_models(&payload);
        assert_eq!(models.len(), 1);
        assert!(models[0].get("creditRate").is_none());
        assert_eq!(multiplier(Some(&json!("模式"))), None);
        assert_eq!(multiplier(Some(&json!("×1.6"))), Some(1.6));
    }
}
