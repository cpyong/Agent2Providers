//! 账号凭证：浏览器登录产物与手填凭证统一成一份 secret，并做临期自动换票。
//!
//! ── secret 形状 ─────────────────────────────────────────────
//! ```json
//! { "accessToken": "…", "refreshToken": "…", "expiresAt": 1730000000000,
//!   "userId": "…", "openid": "…", "tenantId": "…", "name": "…",
//!   "projectUrlId": "…", "serviceAk": "meoo_project_service_v1.…",
//!   "addedAt": 1730000000000 }
//! ```
//! 只有 `serviceAk` 的账号（手填项目服务 AK）能聊天 / 拉模型，但查不了积分
//! 与签到 —— 控制面要的是 OAuth access token，两套鉴权不能互换。
//!
//! ── 身份必须稳定且脱敏 ──────────────────────────────────────
//! `identity` 优先 `userId`，其次 `openid`，最后才用 AK/令牌的 SHA-256 前
//! 16 位。界面上的 `tokenTail` 只给末四位，日志里绝不出现完整令牌。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreAccount, StoreProxy};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{endpoints, oauth};
use crate::http;

/// 提前 2 分钟换票，避免请求打到一半刚好过期。
const REFRESH_EARLY_MS: i64 = 120_000;

#[derive(Clone, Debug)]
pub struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    pub service_ak: String,
    pub project_url_id: String,
    pub proxy: Option<proto::WireProxy>,
}

pub fn pick(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn pick_number(value: &Value, keys: &[&str]) -> i64 {
    keys.iter().find_map(|key| match value.get(*key) {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    }).unwrap_or(0)
}

fn fingerprint(value: &str, prefix: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::from(prefix);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn identity_of(secret: &Value) -> String {
    let uid = pick(secret, &["userId", "user_id", "openid", "uid"]);
    if !uid.is_empty() {
        return format!("meoo-{uid}");
    }
    let token = pick(secret, &["serviceAk", "accessToken", "refreshToken"]);
    if token.is_empty() {
        return String::new();
    }
    fingerprint(&token, "meoo-ak-")
}

pub fn account_view(secret: &Value) -> proto::AccountView {
    let token = pick(secret, &["serviceAk", "accessToken"]);
    let tail = if token.chars().count() > 4 {
        token.chars().rev().take(4).collect::<String>().chars().rev().collect()
    } else {
        String::new()
    };
    json!({
        "name": pick(secret, &["name", "username"]),
        "uid": pick(secret, &["userId", "user_id", "openid"]),
        "tokenTail": tail,
        "project": pick(secret, &["projectUrlId"]),
        "hasServiceAk": !pick(secret, &["serviceAk"]).is_empty(),
        "hasOAuth": !pick(secret, &["accessToken"]).is_empty(),
    })
}

/// 从任意来源解析出一份 secret（不校验远端，落库时不该因为网络抖动失败）。
pub fn parse_input(payload: &Value) -> Result<Value, String> {
    let object = match payload {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.starts_with('{') {
                serde_json::from_str::<Value>(trimmed)
                    .map_err(|error| format!("粘贴的内容不是合法 JSON：{error}"))?
            } else if trimmed.starts_with("meoo_project_service_v1") {
                json!({ "serviceAk": trimmed })
            } else {
                return Err(
                    "请粘贴项目服务 AK（meoo_project_service_v1.…），或浏览器登录后导出的凭证 JSON".to_string(),
                );
            }
        }
        other => other.clone(),
    };
    if !object.is_object() {
        return Err("凭证格式不对：需要 JSON 对象或项目服务 AK 字符串".to_string());
    }
    let service_ak = pick(
        &object,
        &["serviceAk", "meooProjectApiKey", "apiKey", "MEOO_API_KEY"],
    );
    let access_token = pick(&object, &["accessToken", "access_token"]);
    let refresh_token = pick(&object, &["refreshToken", "refresh_token"]);
    if service_ak.is_empty() && access_token.is_empty() && refresh_token.is_empty() {
        return Err(
            "凭证里既没有项目服务 AK 也没有 OAuth token：请用「网页登录」，或粘贴 meoo_project_service_v1.…".to_string(),
        );
    }
    let expires_at = pick_number(&object, &["expiresAt", "expires_at", "accessTokenExpiresAt"]);
    let user_id = pick(&object, &["userId", "user_id", "openid", "uid"]);
    Ok(json!({
        "accessToken": access_token,
        "refreshToken": refresh_token,
        "expiresAt": expires_at,
        "userId": user_id,
        "openid": pick(&object, &["openid"]),
        "tenantId": pick(&object, &["tenantId", "tenant_id"]),
        "name": pick(&object, &["name", "username"]),
        "projectUrlId": pick(&object, &["projectUrlId", "project_url_id"]),
        "serviceAk": service_ak,
        "addedAt": http::now_ms(),
    }))
}

/// 自有页面把 AK / 凭证 JSON 包在 `payload` 字段里；也兼容 `credential`。
/// 网关通用表单可能直接给裸对象，两层都认。
fn unwrap_payload(payload: &Value) -> Value {
    payload
        .get("payload")
        .or_else(|| payload.get("credential"))
        .cloned()
        .unwrap_or_else(|| payload.clone())
}

/// `account.normalize`：手填 / 文本凭证文件两条入口。
pub async fn normalize(
    _ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, String> {
    let raw = match &input.source {
        proto::NormalizeSource::Manual { payload } => payload.clone(),
        proto::NormalizeSource::FileText { content, .. } => Value::String(content.clone()),
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err("Meoo 不需要导入二进制文件，请用「网页登录」或粘贴凭证".to_string())
        }
        proto::NormalizeSource::Desktop => {
            return Err("Meoo 没有可读取的桌面客户端登录态，请用「网页登录」".to_string())
        }
    };
    let mut secret = parse_input(&unwrap_payload(&raw))?;
    if input.name.as_deref().is_some_and(|name| !name.trim().is_empty()) {
        secret["name"] = json!(input.name);
    }
    let display = {
        let name = pick(&secret, &["name"]);
        if !name.is_empty() {
            name
        } else {
            let uid = pick(&secret, &["userId", "openid"]);
            if uid.is_empty() {
                "Meoo 账号".to_string()
            } else {
                format!("Meoo {uid}")
            }
        }
    };
    secret["name"] = json!(display);
    Ok(proto::NormalizedAccount {
        identity: identity_of(&secret),
        public: account_view(&secret),
        secret: secret.clone(),
        desktop: false,
    })
}

/// 浏览器登录完成后，把 OAuth token + 身份 + 项目 AK 组装成可落库的账号。
pub fn from_login(
    token: &oauth::TokenSet,
    identity: &Value,
    project_url_id: &str,
    service_ak: &str,
) -> proto::NormalizedAccount {
    let user_id = pick(identity, &["user_id", "userId", "uid", "openid"]);
    let name = pick(identity, &["name", "username", "nick_name", "display_name"]);
    let display = if name.is_empty() {
        if user_id.is_empty() {
            "Meoo 账号".to_string()
        } else {
            format!("Meoo {user_id}")
        }
    } else {
        name
    };
    let secret = json!({
        "accessToken": token.access_token.as_str(),
        "refreshToken": token.refresh_token.as_str(),
        "expiresAt": http::now_ms() + token.expires_in * 1_000,
        "userId": user_id,
        "openid": token.openid.as_str(),
        "tenantId": token.tenant_id.as_str(),
        "name": display,
        "projectUrlId": project_url_id,
        "serviceAk": service_ak,
        "addedAt": http::now_ms(),
    });
    proto::NormalizedAccount {
        identity: identity_of(&secret),
        public: account_view(&secret),
        secret: secret.clone(),
        desktop: false,
    }
}

/// 账号代理：只认网关已解析的出口和存量静态配置；`Unset` / `Failed` 一律
/// 回落到网关默认出口 —— 与 providers-wpslingxi 同口径，避免把「代理不可用」
/// 误当成「直连」，那会让受限网络下的请求静默失败。
fn proxy_of(record: &StoreAccount) -> Option<proto::WireProxy> {
    match record.proxy.as_ref() {
        Some(StoreProxy::Resolved { proxy }) => Some(proxy.clone()),
        Some(StoreProxy::Static { config }) => serde_json::from_value(config.clone()).ok(),
        _ => None,
    }
}

/// 读账号 + 临期自动换票（换票结果尽力写回，写不回也不影响本次请求）。
pub async fn load(ctx: &PluginCtx, account: &str) -> Result<Credential, String> {
    let record = ctx
        .store_get(super::ID, account)
        .await
        .map_err(|error| format!("读取 Meoo 账号失败：{error}"))?;
    // 落库的 secret 已经是规范形状：这里只读，不能走 parse_input（那个会
    // 重写 addedAt，导致 store_update 的 expect 指纹永远对不上）。
    if !record.secret.is_object() {
        return Err("Meoo 账号凭证格式不对，请重新点「网页登录」".to_string());
    }
    let mut secret = record.secret.clone();
    let proxy = proxy_of(&record);

    let expired = {
        let expires = pick_number(&secret, &["expiresAt"]);
        expires > 0 && expires - REFRESH_EARLY_MS <= http::now_ms()
    };
    let refresh_token = pick(&secret, &["refreshToken"]);
    if expired && !refresh_token.is_empty() {
        let client = http::client(proxy.as_ref(), &ctx.env.proxy);
        match oauth::refresh(&client, endpoints::api_base(), &refresh_token).await {
            Ok(token) => {
                secret["accessToken"] = json!(token.access_token);
                if !token.refresh_token.is_empty() {
                    secret["refreshToken"] = json!(token.refresh_token);
                }
                secret["expiresAt"] = json!(http::now_ms() + token.expires_in * 1_000);
                if !record.id.is_empty() {
                    let _ = ctx
                        .store_update(&record.id, &record.secret, &secret)
                        .await;
                }
                agent2api_provider_sdk::log_info!("[Meoo] access token 已自动续期");
            }
            Err(error) => {
                agent2api_provider_sdk::log_warn!("[Meoo] 自动续期失败：{error}");
            }
        }
    }

    Ok(Credential {
        access_token: pick(&secret, &["accessToken"]),
        refresh_token: pick(&secret, &["refreshToken"]),
        service_ak: pick(&secret, &["serviceAk"]),
        project_url_id: pick(&secret, &["projectUrlId"]),
        proxy,
    })
}

/// 控制面请求必须有 OAuth token；AK-only 账号在这里给出可操作的错误。
pub fn require_access_token(credential: &Credential) -> Result<&str, String> {
    if credential.access_token.is_empty() {
        return Err(
            "这个账号只有项目服务 AK，查积分 / 签到需要 OAuth 登录态：请重新点「网页登录」".to_string(),
        );
    }
    Ok(&credential.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_service_ak_string() {
        let secret = parse_input(&json!("meoo_project_service_v1.abcdef"))
            .expect("secret");
        assert_eq!(pick(&secret, &["serviceAk"]), "meoo_project_service_v1.abcdef");
        assert!(identity_of(&secret).starts_with("meoo-ak-"));
    }

    #[test]
    fn parses_cli_credentials_json() {
        let secret = parse_input(&json!({
            "accessToken": "at", "refreshToken": "rt",
            "accessTokenExpiresAt": 1730000000000i64, "projectUrlId": "p1"
        }))
        .expect("secret");
        assert_eq!(pick(&secret, &["accessToken"]), "at");
        assert_eq!(pick(&secret, &["projectUrlId"]), "p1");
    }

    #[test]
    fn unwraps_page_payload_wrapper() {
        let wrapped = unwrap_payload(&json!({
            "payload": "meoo_project_service_v1.abc", "name": "主账号"
        }));
        assert_eq!(wrapped.as_str(), Some("meoo_project_service_v1.abc"));
        assert_eq!(parse_input(&wrapped).expect("secret")["serviceAk"], "meoo_project_service_v1.abc");
        let plain = unwrap_payload(&json!({ "apiKey": "ak" }));
        assert_eq!(plain["apiKey"], "ak");
    }

    #[test]
    fn rejects_empty_payload() {
        assert!(parse_input(&json!({})).is_err());
        assert!(parse_input(&json!("not-a-token")).is_err());
    }

    #[test]
    fn identity_prefers_user_id_and_view_hides_token() {
        let secret = json!({
            "userId": "u1", "serviceAk": "meoo_project_service_v1.0123456789"
        });
        assert_eq!(identity_of(&secret), "meoo-u1");
        let view = account_view(&secret);
        assert_eq!(view["tokenTail"], "6789");
        assert!(view.to_string().len() < 300);
        assert!(!view.to_string().contains("0123456789"));
    }

    #[test]
    fn proxy_of_accepts_only_resolved_and_static() {
        let wire: proto::WireProxy = serde_json::from_value(json!({
            "protocol": "http", "host": "127.0.0.1", "port": 7890
        }))
        .expect("wire proxy");
        let record = |proxy: Option<StoreProxy>| StoreAccount {
            id: "acc-1".to_string(),
            provider: crate::meoo::ID.to_string(),
            secret: json!({}),
            public: json!({}),
            proxy,
        };
        assert_eq!(
            proxy_of(&record(Some(StoreProxy::Resolved {
                proxy: wire.clone()
            }))),
            Some(wire.clone())
        );
        let config = serde_json::to_value(&wire).expect("serialize");
        assert_eq!(
            proxy_of(&record(Some(StoreProxy::Static { config }))),
            Some(wire)
        );
        // Unset / Failed 必须回落到网关默认出口，不能被当成直连。
        assert!(proxy_of(&record(Some(StoreProxy::Unset))).is_none());
        assert!(
            proxy_of(&record(Some(StoreProxy::Failed {
                message: "dns failed".to_string()
            })))
            .is_none()
        );
        assert!(proxy_of(&record(None)).is_none());
    }

    #[test]
    fn from_login_builds_stable_identity() {
        let token = oauth::TokenSet {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            expires_in: 3600,
            openid: "open-1".to_string(),
            tenant_id: "t".to_string(),
            scope: "cli.compat".to_string(),
        };
        let account = from_login(&token, &json!({"user_id": "u1", "name": "小喵"}), "p1", "ak");
        assert_eq!(account.identity, "meoo-u1");
        assert_eq!(account.public["name"], "小喵");
        assert_eq!(account.secret["projectUrlId"], "p1");
    }
}
