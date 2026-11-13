//! 控制面与 AI 网关的 HTTP 调用。
//!
//! 两套鉴权不能混：`/open/v1/cli-compat/…` 用 OAuth access token，
//! `/meoo-ai/compatible-mode/v1/…` 用项目服务 AK。把 AK 发到控制面会 401，
//! 把 access token 发到网关会 `SERVICE_AK_FORMAT_INVALID`。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{credentials, endpoints, oauth};
use crate::http;

fn control_headers(access_token: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {access_token}")),
        ("X-Meoo-Client".to_string(), "cli".to_string()),
        (
            "X-Meoo-CLI-Version".to_string(),
            oauth::CLIENT_VERSION.to_string(),
        ),
    ]
}

/// 控制面请求：检查 HTTP 状态、拆 `{success,data}` 信封、401 给人话。
pub async fn control(
    client: &reqwest::Client,
    method: reqwest::Method,
    path: &str,
    access_token: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = endpoints::url(endpoints::api_base(), path);
    let (status, raw) = http::request_json(
        client,
        method,
        &url,
        &control_headers(access_token),
        body,
    )
    .await?;
    if status == 401 || status == 403 {
        return Err(format!(
            "Meoo 登录态已失效（{status}），请点「网页登录」重新授权"
        ));
    }
    if !(200..300).contains(&status) {
        return Err(http::error_message(&raw, status));
    }
    Ok(http::unwrap_envelope(raw))
}

pub async fn identity(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Value, String> {
    control(client, reqwest::Method::GET, endpoints::EP_IDENTITY, access_token, None).await
}

pub async fn points(client: &reqwest::Client, access_token: &str) -> Result<Value, String> {
    control(client, reqwest::Method::GET, endpoints::EP_POINTS, access_token, None).await
}

pub async fn entitlement(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Value, String> {
    control(client, reqwest::Method::GET, endpoints::EP_ENTITLEMENT, access_token, None).await
}

/// 项目列表：上游给过 `list` / `items` / `projects` / 裸数组四种形状。
pub fn parse_projects(value: &Value) -> Vec<Value> {
    if let Some(array) = value.as_array() {
        return array.clone();
    }
    ["list", "items", "projects"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
        .cloned()
        .unwrap_or_default()
}

pub fn project_id(entry: &Value) -> String {
    credentials::pick(entry, &["project_url_id", "projectUrlId", "urlId", "url_id", "id"])
}

pub async fn projects(client: &reqwest::Client, access_token: &str) -> Result<Vec<Value>, String> {
    let value = control(client, reqwest::Method::GET, endpoints::EP_PROJECTS, access_token, None).await?;
    Ok(parse_projects(&value))
}

fn service_ak(environment: &Value) -> String {
    let direct = credentials::pick(environment, &["meooProjectApiKey", "meoo_project_api_key"]);
    if !direct.is_empty() {
        return direct;
    }
    environment
        .get("instance")
        .map(|instance| credentials::pick(instance, &["meooProjectApiKey", "meoo_project_api_key"]))
        .unwrap_or_default()
}

pub async fn cloud_environment(
    client: &reqwest::Client,
    access_token: &str,
    project: &str,
) -> Result<Value, String> {
    control(
        client,
        reqwest::Method::GET,
        &endpoints::project_cloud_environment(project),
        access_token,
        None,
    )
    .await
}

/// 取（必要时现开通）项目服务 AK。没有 AK 时 `/models` 与聊天都发不出去。
pub async fn resolve_service_ak(
    ctx: &PluginCtx,
    credential: &credentials::Credential,
) -> Result<String, String> {
    if !credential.service_ak.is_empty() {
        return Ok(credential.service_ak.clone());
    }
    let token = credentials::require_access_token(credential)?;
    let client = http::client(credential.proxy.as_ref(), &ctx.env.proxy);

    let mut project = credential.project_url_id.clone();
    if project.is_empty() {
        let me = identity(&client, token).await?;
        project = credentials::pick(&me, &["project_url_id", "projectUrlId"]);
    }
    if project.is_empty() {
        let list = projects(&client, token).await?;
        project = list
            .iter()
            .map(project_id)
            .find(|id| !id.is_empty())
            .unwrap_or_default();
    }
    if project.is_empty() {
        return Err(
            "这个 Meoo 账号还没有项目：请先在 meoo.com 创建一个项目，或手填项目服务 AK".to_string(),
        );
    }

    let mut ak = cloud_environment(&client, token, &project)
        .await
        .ok()
        .map(|value| service_ak(&value))
        .unwrap_or_default();
    if ak.is_empty() {
        // 与官方 CLI / dsh-meoo 一致：没开通就替用户开一次，再读一遍。
        let body = json!({ "description": "Agent2API: AI 服务" });
        let _ = control(
            &client,
            reqwest::Method::POST,
            &endpoints::project_cloud(&project),
            token,
            Some(&body),
        )
        .await;
        ak = cloud_environment(&client, token, &project)
            .await
            .ok()
            .map(|value| service_ak(&value))
            .unwrap_or_default();
    }
    if ak.is_empty() {
        return Err(format!(
            "项目 {project} 还没开通 Meoo AI 服务：请在 meoo.com 打开该项目 →「云服务」开通一次，或手填项目服务 AK"
        ));
    }
    Ok(ak)
}

/// 网关模型目录：OpenAI 风格 `{object:"list",data:[{id…}]}`，也认顶层 `models`。
pub fn parse_models(value: &Value) -> Vec<Value> {
    let list = value
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| value.get("data").and_then(Value::as_array))
        .or_else(|| value.as_array());
    list.cloned().unwrap_or_default()
}

pub async fn gateway_models(
    ctx: &PluginCtx,
    credential: &credentials::Credential,
) -> Result<Vec<Value>, String> {
    let ak = resolve_service_ak(ctx, credential).await?;
    let client = http::client(credential.proxy.as_ref(), &ctx.env.proxy);
    let url = endpoints::url(endpoints::gateway_base(), endpoints::EP_MODELS);
    let headers = vec![
        ("Authorization".to_string(), format!("Bearer {ak}")),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    let (status, body) =
        http::request_json(&client, reqwest::Method::GET, &url, &headers, None).await?;
    if !(200..300).contains(&status) {
        return Err(http::error_message(&body, status));
    }
    Ok(parse_models(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_accepts_four_shapes() {
        assert_eq!(parse_projects(&json!({"list": [{"id": "p1"}]})).len(), 1);
        assert_eq!(parse_projects(&json!({"items": [{"id": "p1"}]})).len(), 1);
        assert_eq!(parse_projects(&json!({"projects": [{"id": "p1"}]})).len(), 1);
        assert_eq!(parse_projects(&json!([{"id": "p1"}])).len(), 1);
        assert!(parse_projects(&json!({})).is_empty());
    }

    #[test]
    fn project_id_accepts_snake_and_camel() {
        assert_eq!(project_id(&json!({"project_url_id": "a"})), "a");
        assert_eq!(project_id(&json!({"urlId": "b"})), "b");
        assert_eq!(project_id(&json!({})), "");
    }

    #[test]
    fn service_ak_reads_top_level_and_instance() {
        assert_eq!(
            service_ak(&json!({"meooProjectApiKey": "ak1"})),
            "ak1"
        );
        assert_eq!(
            service_ak(&json!({"instance": {"meooProjectApiKey": "ak2"}})),
            "ak2"
        );
        assert_eq!(service_ak(&json!({})), "");
    }

    #[test]
    fn models_accepts_openai_envelope() {
        let list = parse_models(&json!({"object": "list", "data": [{"id": "qwen3.8-flash"}]}));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "qwen3.8-flash");
    }
}
