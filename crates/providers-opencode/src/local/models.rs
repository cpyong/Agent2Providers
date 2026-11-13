//! 本机服务的模型清单（`GET /api/model`）。
//!
//! v2 API 返回 `{ data: [{ id, modelID, providerID, type, name, … }] }`。
//! 同一个 id 可能同时挂在 `opencode`（官方免费车道）和 `zen-gate`（本机
//! 聚合网关）下，例如 `beam-free`。对外只暴露**裸 id**，内部记一张
//! `id → providerID` 映射，`opencode` 优先；客户端也可以显式写
//! `zen-gate/kilo-auto/free` 来选另一家。

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::Value;

use super::api;

const TTL_MS: i64 = 5 * 60 * 1000;

struct Cache {
    models: Vec<Value>,
    providers: HashMap<String, String>,
    provider_set: BTreeSet<String>,
    default_model: String,
    refreshed_at: i64,
}

fn cache() -> &'static Mutex<Option<Cache>> {
    static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 解析一次 `/api/model` 响应并写入缓存，返回模型数量。
pub fn store(payload: &Value) -> usize {
    let Some(items) = payload.get("data").and_then(Value::as_array) else {
        return 0;
    };
    let mut models: Vec<Value> = Vec::new();
    let mut providers: HashMap<String, String> = HashMap::new();
    let mut provider_set: BTreeSet<String> = BTreeSet::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut default_model = String::new();

    for item in items {
        if item
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "chat")
        {
            continue;
        }
        let id = item
            .get("modelID")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| item.get("id").and_then(Value::as_str))
            .map(str::trim)
            .unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let provider = item
            .get("providerID")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("opencode");
        provider_set.insert(provider.to_string());
        if provider == "opencode" && default_model.is_empty() {
            default_model = id.to_string();
        }
        // 同名 id 优先官方免费车道
        let take = match providers.get(id) {
            None => true,
            Some(current) => current != "opencode" && provider == "opencode",
        };
        if take {
            providers.insert(id.to_string(), provider.to_string());
        }
        if seen.insert(id.to_string()) {
            models.push(serde_json::json!({ "id": id }));
        }
    }
    if default_model.is_empty() {
        default_model = seen.into_iter().next().unwrap_or_default();
    }
    let count = models.len();
    if count == 0 {
        return 0;
    }
    if let Ok(mut guard) = cache().lock() {
        *guard = Some(Cache {
            models,
            providers,
            provider_set,
            default_model,
            refreshed_at: crate::http::now_ms(),
        });
    }
    count
}

/// 当前清单。
pub fn snapshot() -> proto::ModelList {
    let mut models: Vec<Value> = Vec::new();
    let mut refreshed_at = 0i64;
    let mut remote = false;
    if let Ok(guard) = cache().lock() {
        if let Some(entry) = guard.as_ref() {
            remote = true;
            refreshed_at = entry.refreshed_at;
            models = entry.models.clone();
        }
    }
    proto::ModelList {
        models,
        meta: proto::CatalogMeta {
            remote,
            refreshed_at,
        },
    }
}

/// `model` → `(providerID, modelID)`。
///
/// 空串用清单里的默认免费模型；找不到时如实报错（不猜一个名字发出去）。
pub fn resolve(model: &str) -> Result<(String, String), String> {
    let Ok(guard) = cache().lock() else {
        return Err("模型清单缓存不可用".to_string());
    };
    let Some(entry) = guard.as_ref() else {
        return Err("还没有拉到本机 opencode 的模型清单，请先在插件页刷新模型或添加账号".to_string());
    };
    let model = model.trim();
    if model.is_empty() {
        if entry.default_model.is_empty() {
            return Err("本机 opencode 服务没有可用的默认模型".to_string());
        }
        return Ok(("opencode".to_string(), entry.default_model.clone()));
    }
    if let Some((prefix, rest)) = model.split_once('/') {
        if entry.provider_set.contains(prefix) && !rest.is_empty() {
            return Ok((prefix.to_string(), rest.to_string()));
        }
    }
    match entry.providers.get(model) {
        Some(provider) => Ok((provider.clone(), model.to_string())),
        None => Err(format!(
            "模型 `{model}` 不在本机 opencode 服务的清单里（刷新模型清单后重试）"
        )),
    }
}

/// 拉一次模型清单。
pub async fn refresh(server: &api::Server, force: bool) -> proto::RefreshOutcome {
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
    let client = api::control();
    let (status, body) = match api::request_json(
        &client,
        server,
        reqwest::Method::GET,
        "/api/model",
        None,
    )
    .await
    {
        Ok(found) => found,
        Err(reason) => return proto::RefreshOutcome::failed(reason),
    };
    if !(200..300).contains(&status) {
        return proto::RefreshOutcome::failed(api::probe_error(status));
    }
    let count = store(&body);
    if count == 0 {
        return proto::RefreshOutcome::failed("本机 opencode 服务没有列出任何可用模型");
    }
    proto::RefreshOutcome::refreshed(count)
}

#[cfg(test)]
mod tests {
    use super::store;
    use serde_json::json;

    #[test]
    fn parses_models_and_prefers_opencode_provider() {
        let count = store(&json!({
            "data": [
                { "id": "beam-free", "modelID": "beam-free", "providerID": "zen-gate", "type": "chat" },
                { "id": "beam-free", "modelID": "beam-free", "providerID": "opencode", "type": "chat" },
                { "id": "iceman-free", "modelID": "iceman-free", "providerID": "opencode", "type": "chat" },
                { "id": "kilo-auto/free", "modelID": "kilo-auto/free", "providerID": "zen-gate", "type": "chat" },
                { "id": "embedding", "modelID": "embedding", "providerID": "opencode", "type": "embedding" }
            ]
        }));
        assert_eq!(count, 3);
        // 缓存是进程级静态量，断言放到 resolve 上验证优先级
        let (provider, id) = super::resolve("beam-free").unwrap_or_default();
        assert_eq!(provider, "opencode");
        assert_eq!(id, "beam-free");
        assert!(super::resolve("embedding").is_err());
    }
}
