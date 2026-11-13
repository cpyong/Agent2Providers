//! OAuth 2.0 授权码 + PKCE（与官方 CLI 完全同形）。
//!
//! ── 为什么走 PKCE 而不是抓 Cookie ───────────────────────────
//! Meoo 官方给 CLI 的就是这套公开客户端流程：本机起一个 loopback 监听器，
//! 浏览器打开 `/oauth/authorize`，用户点一次授权，回调带 code 回来，再用
//! code_verifier 换 token。抓浏览器 Cookie 要处理站点隔离、CDP、游客态，
//! 而且拿不到 `/open/v1/cli-compat` 认可的 Bearer token。
//!
//! 关键点：`redirect_uri` 用**动态端口**，`/oauth/token` 换票时必须把同一个
//! 字符串原样回传；`scope` 是 `cli.compat`（点号），API 路径是
//! `cli-compat`（连字符），两个字符串不一样，别互相抄。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::endpoints;
use crate::http;

pub const CLIENT_VERSION: &str = "0.5.8";

#[derive(Clone, Debug)]
pub struct Pkce {
    pub state: String,
    pub verifier: String,
    pub challenge: String,
}

#[derive(Clone, Debug)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub openid: String,
    pub tenant_id: String,
    pub scope: String,
}

fn random_bytes(length: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; length];
    getrandom::getrandom(&mut bytes).map_err(|error| format!("生成随机数失败：{error}"))?;
    Ok(bytes)
}

fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn new_pkce() -> Result<Pkce, String> {
    let state = b64url(&random_bytes(32)?);
    let verifier = b64url(&random_bytes(32)?);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = b64url(&hasher.finalize());
    Ok(Pkce { state, verifier, challenge })
}

/// query / form 值编码（只留 unreserved 字符，其余一律 %XX）。
pub fn encode(value: &str) -> String {
    http::encode_component(value)
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn authorize_url(api_base: &str, redirect_uri: &str, pkce: &Pkce) -> String {
    let query = form(&[
        ("client_id", endpoints::client_id(api_base)),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", endpoints::OAUTH_SCOPE),
        ("state", &pkce.state),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
    ]);
    format!(
        "{}{}?{}",
        api_base.trim_end_matches('/'),
        endpoints::OAUTH_AUTHORIZE,
        query
    )
}

fn headers() -> Vec<(String, String)> {
    vec![
        ("X-Meoo-Client".to_string(), "cli".to_string()),
        ("X-Meoo-CLI-Version".to_string(), CLIENT_VERSION.to_string()),
    ]
}

async fn token_request(
    client: &reqwest::Client,
    api_base: &str,
    pairs: &[(&str, &str)],
) -> Result<TokenSet, String> {
    let url = endpoints::url(api_base, endpoints::OAUTH_TOKEN);
    let (status, body) = http::request_form(client, &url, &headers(), &form(pairs)).await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "OAuth 换票失败：{}",
            http::error_message(&body, status)
        ));
    }
    parse_token(&body)
}

pub fn parse_token(value: &Value) -> Result<TokenSet, String> {
    let text = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let access_token = text(&["access_token"]);
    if access_token.is_empty() {
        return Err("OAuth 响应里没有 access_token".to_string());
    }
    let expires_in = value
        .get("expires_in")
        .and_then(|number| number.as_i64().or_else(|| number.as_str()?.trim().parse().ok()))
        .unwrap_or(3_600);
    let token = TokenSet {
        access_token,
        refresh_token: text(&["refresh_token"]),
        expires_in,
        openid: text(&["openid"]),
        tenant_id: text(&["tenant_id"]),
        scope: text(&["scope"]),
    };
    // 有些部署不回 scope，空值不算异常；回了却不含 cli.compat 就说明用户
    // 授权的不是 CLI 通道，控制面随后必然 403 —— 在这里把原因说清楚。
    if !token.scope.is_empty()
        && !token
            .scope
            .split(|c: char| c.is_whitespace() || c == ',')
            .any(|part| part.eq_ignore_ascii_case(endpoints::OAUTH_SCOPE))
    {
        agent2api_provider_sdk::log_warn!(
            "[Meoo] OAuth 返回的 scope 不含 {}，控制面接口可能返回 403（实际 scope：{}）",
            endpoints::OAUTH_SCOPE,
            token.scope
        );
    }
    Ok(token)
}

pub async fn exchange_code(
    client: &reqwest::Client,
    api_base: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenSet, String> {
    token_request(
        client,
        api_base,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", endpoints::client_id(api_base)),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
    )
    .await
}

pub async fn refresh(
    client: &reqwest::Client,
    api_base: &str,
    refresh_token: &str,
) -> Result<TokenSet, String> {
    token_request(
        client,
        api_base,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", endpoints::client_id(api_base)),
            ("refresh_token", refresh_token),
        ],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pkce_challenge_matches_verifier_and_is_url_safe() {
        let pkce = new_pkce().expect("pkce");
        let mut hasher = Sha256::new();
        hasher.update(pkce.verifier.as_bytes());
        assert_eq!(pkce.challenge, URL_SAFE_NO_PAD.encode(hasher.finalize()));
        assert!(!pkce.challenge.contains('+'));
        assert!(!pkce.challenge.contains('/'));
        assert!(!pkce.challenge.contains('='));
        assert_ne!(pkce.state, pkce.verifier);
    }

    #[test]
    fn authorize_url_carries_all_parameters() {
        let pkce = Pkce {
            state: "s t".to_string(),
            verifier: "v".to_string(),
            challenge: "c".to_string(),
        };
        let url = authorize_url("https://meoo.com", "http://127.0.0.1:9/oauth/callback", &pkce);
        assert!(url.starts_with("https://meoo.com/oauth/authorize?"));
        assert!(url.contains("client_id=meoo_app_tdWlRbpvAxN1EjmZ8XICxdTh"));
        assert!(url.contains("scope=cli.compat"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=s%20t"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Foauth%2Fcallback"));
    }

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(form(&[("a b", "x&y=z")]), "a%20b=x%26y%3Dz");
    }

    #[test]
    fn token_parser_accepts_string_numbers_and_rejects_empty() {
        let token = parse_token(&json!({
            "access_token": "at", "refresh_token": "rt", "expires_in": "3600",
            "token_type": "Bearer", "scope": "cli.compat", "openid": "o", "tenant_id": "t"
        }))
        .expect("token");
        assert_eq!(token.expires_in, 3600);
        assert_eq!(token.scope, "cli.compat");
        assert!(parse_token(&json!({"error": "invalid_grant"})).is_err());
    }
}
