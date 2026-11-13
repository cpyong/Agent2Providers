//! 免费模型目录。
//!
//! ── 数据源：**实际转发通道**的 `/v1/models` ──────────────────
//! 曾经用 `https://models.opencode.ai/api.json`（能力 / 价格目录）按零价筛
//! 模型，但那份目录是给「所有 provider」用的：实测它筛出 51 个免费模型，
//! 而匿名 `inference` 通道的 `/v1/models` 只列 84 个模型、其中真正零价的
//! 只有 **12** 个，两边交集仅 12 —— 广告出来的模型有 39 个上游会回
//! `Model is unavailable.`。目录与放行清单必须是同一份数据源，所以改为：
//!
//! ```text
//!   GET https://opencode.ai/inference/openai/v1/models
//!     → { data: [ { id, cost: { text: { input: { price }, output: { price } } } } ] }
//! ```
//!
//! 免费判定：`cost.text.input.price == 0 && cost.text.output.price == 0`，
//! 或 id 含 `free`。实测 84 个模型里恰好 12 个零价（11 个名字带 free +
//! `big-pickle`），与本机 opencode `/api/model` 列出的免费模型完全一致。
//! 全站能力目录里零价模型有 566 个，因此**不能**再用它做白名单。
//!
//! ── 不编模型名 ──────────────────────────────────────────────
//! 目录拉不到就如实报失败并保留上一次缓存；不做「看起来像」的静态兜底。

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::Value;

use super::endpoints;

/// 目录缓存寿命（毫秒）。
const TTL_MS: i64 = 5 * 60 * 1000;

struct Entry {
    ids: Vec<String>,
    refreshed_at: i64,
}

fn cache() -> &'static Mutex<Option<Entry>> {
    static CACHE: OnceLock<Mutex<Option<Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 自定义基址的上游模型清单缓存（键 = 基址）。
struct CustomEntry {
    ids: BTreeSet<String>,
    refreshed_at: i64,
}

fn custom_cache() -> &'static Mutex<HashMap<String, CustomEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CustomEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 当前免费模型清单。
pub fn snapshot() -> proto::ModelList {
    let mut ids: Vec<String> = Vec::new();
    let mut refreshed_at = 0i64;
    let mut remote = false;
    if let Ok(guard) = cache().lock() {
        if let Some(entry) = guard.as_ref() {
            remote = true;
            refreshed_at = entry.refreshed_at;
            ids = entry.ids.clone();
        }
    }
    proto::ModelList {
        models: ids
            .into_iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect(),
        meta: proto::CatalogMeta {
            remote,
            refreshed_at,
        },
    }
}

/// 请求的模型是否在免费集合里。
///
/// 目录还没拉到时退回名字规则（含 `free`）——这样「刚装好插件、还没刷新
/// 目录」的请求不会被误杀；拉过目录之后以目录为准。
pub fn is_free_model(model: &str) -> bool {
    let name_free = model.to_ascii_lowercase().contains("free");
    if let Ok(guard) = cache().lock() {
        if let Some(entry) = guard.as_ref() {
            return entry.ids.iter().any(|id| id == model);
        }
    }
    name_free
}

/// 自定义基址的放行判定：读上游自己的 `/v1/models`。
///
/// 自定义基址（本机 `zen-gate` 网关 / 第三方聚合）的模型集合与官方目录不同，
/// `kilo-auto/free`、`nvidia/...:free` 这类聚合 id 在官方清单里查不到，
/// 套官方白名单会误杀。官方基址不用走这里，`build_chat` 直接查免费集合。
pub async fn allowed_for_custom(
    client: &reqwest::Client,
    key: &str,
    base: &str,
    model: &str,
) -> bool {
    let base_key = base.trim().trim_end_matches('/').to_string();
    let now = crate::http::now_ms();
    if let Ok(guard) = custom_cache().lock() {
        if let Some(entry) = guard.get(&base_key) {
            if now - entry.refreshed_at < TTL_MS {
                return entry.ids.contains(model);
            }
        }
    }

    let url = endpoints::models_url(&base_key);
    let headers = super::identity::models_headers(key, &base_key);
    match crate::http::get_json(client, &url, &headers).await {
        Ok((status, payload)) if (200..300).contains(&status) => {
            let ids = model_ids(&payload);
            let hit = ids.contains(model);
            if let Ok(mut guard) = custom_cache().lock() {
                guard.insert(
                    base_key,
                    CustomEntry {
                        ids,
                        refreshed_at: now,
                    },
                );
            }
            hit
        }
        // 清单拉不到时不阻断：用户自己配的网关由用户负责
        _ => true,
    }
}

/// 从 `/v1/models` 响应里取模型 id 集合（兼容 `data` / `models` / 裸数组）。
pub fn model_ids(payload: &Value) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let items = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.get("models").and_then(Value::as_array))
        .or_else(|| payload.as_array());
    let Some(items) = items else {
        return ids;
    };
    for item in items {
        let id = item
            .as_str()
            .or_else(|| item.get("id").and_then(Value::as_str))
            .map(str::trim)
            .unwrap_or("");
        if !id.is_empty() {
            ids.insert(id.to_string());
        }
    }
    ids
}

/// 从 inference `/v1/models` 里筛出免费模型 id。
///
/// 两种零价写法都认：OpenCode 的 `cost.text.{input,output}.price`，以及
/// 聚合网关常用的扁平 `cost.{input,output}`。名字带 `free` 的一律收下
/// （上游目录里 `*-free` 就是免费车道标记）。
pub fn free_model_ids(payload: &Value) -> Vec<String> {
    let Some(items) = payload.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        if zero_cost(item) || id.to_ascii_lowercase().contains("free") {
            ids.insert(id.to_string());
        }
    }
    ids.into_iter().collect()
}

/// 单条模型是否零价。
fn zero_cost(model: &Value) -> bool {
    let Some(cost) = model.get("cost") else {
        return false;
    };
    let (Some(input), Some(output)) = (price(cost, "input"), price(cost, "output")) else {
        return false;
    };
    input == 0.0 && output == 0.0
}

/// 取价格：优先 `cost.text.<side>.price`，兼容 `cost.<side>.price` /
/// `cost.<side>` 直接是数字。
fn price(cost: &Value, side: &str) -> Option<f64> {
    let nested = cost
        .get("text")
        .and_then(|text| text.get(side))
        .or_else(|| cost.get(side));
    let value = nested?;
    value
        .get("price")
        .and_then(Value::as_f64)
        .or_else(|| value.as_f64())
}

/// 拉一次免费模型目录（公开端点，不需要账号）。
pub async fn refresh(client: &reqwest::Client, force: bool) -> proto::RefreshOutcome {
    let now = crate::http::now_ms();
    if !force {
        if let Ok(guard) = cache().lock() {
            if let Some(entry) = guard.as_ref() {
                if now - entry.refreshed_at < TTL_MS {
                    return proto::RefreshOutcome::unchanged();
                }
            }
        }
    }

    // 与真实转发同一条车道：匿名请求头 + CLI UA。
    let headers = super::identity::models_headers("", endpoints::ANON_BASE);
    let (status, payload) =
        match crate::http::get_json(client, endpoints::ANON_MODELS_URL, &headers).await {
            Ok(found) => found,
            Err(reason) => return proto::RefreshOutcome::failed(reason),
        };
    if !(200..300).contains(&status) {
        return proto::RefreshOutcome::failed(format!("模型清单返回 HTTP {status}"));
    }
    let ids = free_model_ids(&payload);
    if ids.is_empty() {
        return proto::RefreshOutcome::failed("上游模型清单里没有找到免费模型");
    }

    let count = ids.len();
    if let Ok(mut guard) = cache().lock() {
        *guard = Some(Entry {
            ids,
            refreshed_at: now,
        });
    }
    proto::RefreshOutcome::refreshed(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inference_catalog_only_keeps_zero_cost_models() {
        let payload = json!({
            "data": [
                { "id": "big-pickle", "cost": { "text": { "input": { "price": 0 }, "output": { "price": 0 } } } },
                { "id": "glm-5.3-flash-free", "cost": { "text": { "input": { "price": 0 }, "output": { "price": 0 } } } },
                { "id": "claude-opus-5", "cost": { "text": { "input": { "price": 5 }, "output": { "price": 25 } } } },
                { "id": "kilo-auto/free" }
            ]
        });
        let ids = free_model_ids(&payload);
        assert_eq!(
            ids,
            vec![
                "big-pickle".to_string(),
                "glm-5.3-flash-free".to_string(),
                "kilo-auto/free".to_string()
            ]
        );
    }

    #[test]
    fn accepts_flat_cost_shape() {
        assert!(zero_cost(&json!({ "cost": { "input": 0, "output": 0 } })));
        assert!(!zero_cost(&json!({ "cost": { "input": 0, "output": 1 } })));
        assert!(!zero_cost(&json!({})));
    }

    #[test]
    fn model_ids_handles_three_shapes() {
        assert_eq!(model_ids(&json!({ "data": [{ "id": "a" }] })).len(), 1);
        assert_eq!(model_ids(&json!({ "models": ["a", { "id": "b" }] })).len(), 2);
        assert_eq!(model_ids(&json!(["a"])).len(), 1);
    }
}
