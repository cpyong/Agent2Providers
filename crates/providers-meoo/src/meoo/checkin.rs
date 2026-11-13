//! 签到 / 积分：参照 WPS 灵犀的取证纪律，绝不把 HTTP 200 当成功。
//!
//! ── 先说清楚上游是什么 ──────────────────────────────────────
//! Meoo **没有**独立的「签到 / 领取」接口。官方文档
//! `docs.meoo.com/coindesc` 的原话是「所有用户每日首次登录系统，均可免费
//! 获赠 2000 积分」，官网前端只有一个 `daily-login-grant-toast`（登录赠
//! 积分到账提示），没有任务中心的 claim 按钮/端点。
//!
//! 所以这里的「签到」实现为：
//!   ① 领前读一次 `/account/points` 余额；
//!   ② 用 refresh_token 走一次 `/oauth/token` 续期（等价于一次登录），
//!      再打一次 `/account/identity` 触发会话；
//!   ③ 领后最多回读两轮余额（间隔 1.2 秒，容忍发放延迟）；
//!   ④ 只有余额**真的涨了**才报 `Claimed`；没涨但权益里确实有
//!      `daily_login` 档位 → `Done`（今日已领 / 上游已判重）；
//!      连余额都读不到 → `Failed`，绝不假装成功。
//!
//! 与 wpslingxi 的差别：那边有真实 claim 端点，所以还要回查任务状态；
//! 这边没有端点，余额差是唯一可信证据。`claim.status` 是只读的，绝不换票
//! 或触发登录。

use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{client, credentials, endpoints, oauth};
use crate::http;

fn report(
    outcome: proto::CheckinOutcome,
    credit: Option<String>,
    message: impl Into<String>,
) -> proto::CheckinReport {
    proto::CheckinReport {
        outcome,
        credit,
        streak: None,
        message: Some(message.into()),
    }
}

/// 上游数字经常写成字符串，两种都认。
pub fn number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().replace(',', "").parse::<f64>().ok(),
        _ => None,
    };
    parsed.filter(|value| value.is_finite())
}

/// `/account/points` 的余额位置实测（CLI `account.js`）是
/// `account.availableBalance`，兼容几种旧形状。
pub fn balance(points: &Value) -> Option<f64> {
    number(points.get("account").and_then(|account| account.get("availableBalance")))
        .or_else(|| number(points.get("availableBalance")))
        .or_else(|| number(points.get("account").and_then(|a| a.get("totalRemaining"))))
        .or_else(|| number(points.get("balance")))
        .or_else(|| number(points.get("totalRemaining")))
}

pub fn daily_quota(entitlement: &Value) -> Option<f64> {
    let benefits = entitlement
        .get("benefits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for benefit in &benefits {
        let kind = credentials::pick(benefit, &["typeCode", "type_code", "code"]);
        if kind == "daily_login" {
            if let Some(quota) = number(benefit.get("quota")) {
                return Some(quota);
            }
        }
    }
    entitlement
        .get("userPointInfo")
        .and_then(|info| info.get("dailyLoginGrant"))
        .and_then(|value| number(Some(value)))
}

fn today_granted(entitlement: &Value) -> Option<bool> {
    ["todayLoginGranted", "dailyLoginGranted", "daily_login_granted"]
        .iter()
        .find_map(|key| entitlement.get(*key).and_then(Value::as_bool))
}

async fn read_points(
    ctx: &PluginCtx,
    credential: &credentials::Credential,
    token: &str,
) -> Result<Value, String> {
    let client = http::client(credential.proxy.as_ref(), &ctx.env.proxy);
    client::points(&client, token).await
}

/// `usage.query`：网关只读顶层 `available`，其余是给界面看的明细。
pub async fn usage(ctx: &PluginCtx, account: &str) -> Result<proto::UsageReport, String> {
    let credential = credentials::load(ctx, account).await?;
    let token = credentials::require_access_token(&credential)?;
    let points = read_points(ctx, &credential, token).await?;
    let available = balance(&points).unwrap_or(0.0);
    let total = number(points.get("account").and_then(|a| a.get("totalGranted")))
        .or_else(|| number(points.get("totalGranted")))
        .unwrap_or(available);
    let used = number(points.get("account").and_then(|a| a.get("totalConsumed")))
        .or_else(|| number(points.get("totalConsumed")))
        .unwrap_or_else(|| (total - available).max(0.0));
    Ok(json!({
        "available": available,
        "totalLeft": available,
        "balance": available,
        "total": total,
        "used": used,
        "unit": "积分",
        "plan": "Meoo",
        "wallets": [{
            "id": "points",
            "label": "积分",
            "available": available,
            "total": total,
            "used": used,
        }],
        "quotaSources": [{
            "id": "account",
            "label": "Meoo 积分",
            "available": available,
            "total": total,
        }],
        "credits": [{
            "kind": "daily_login",
            "available": available,
        }],
    }))
}

/// 只读状态：不换票、不触发登录，签到中心每次打开都会问一次。
pub async fn status(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let credential = credentials::load(ctx, account).await?;
    let Ok(token) = credentials::require_access_token(&credential) else {
        return Ok(json!({
            "active": false,
            "todayCheckedIn": Value::Null,
            "activityName": "每日登录赠送积分",
            "message": "该账号只有项目服务 AK，无法查询签到状态；请重新用网页登录",
        }));
    };
    let client = http::client(credential.proxy.as_ref(), &ctx.env.proxy);
    let points = client::points(&client, token).await?;
    let entitlement = client::entitlement(&client, token).await.unwrap_or(Value::Null);
    let quota = daily_quota(&entitlement).unwrap_or(endpoints::DAILY_LOGIN_GRANT);
    Ok(json!({
        "active": true,
        "todayCheckedIn": today_granted(&entitlement).unwrap_or(false),
        "available": balance(&points).unwrap_or(0.0),
        "todayCredit": quota,
        "activityName": "每日登录赠送积分",
        "note": "Meoo 没有独立签到接口；积分由服务端在每日首次登录时发放",
    }))
}

/// `checkin.claim`：登录续期 + 余额差取证。
pub async fn checkin(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, String> {
    let credential = credentials::load(ctx, account).await?;
    let Ok(token) = credentials::require_access_token(&credential) else {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            None,
            "该账号只有项目服务 AK，签到需要 OAuth 登录态：请重新点「网页登录」",
        ));
    };
    let client = http::client(credential.proxy.as_ref(), &ctx.env.proxy);

    let before = match client::points(&client, token).await {
        Ok(points) => balance(&points),
        Err(error) => {
            return Ok(report(
                proto::CheckinOutcome::Failed,
                None,
                format!("领前读取积分失败：{error}"),
            ))
        }
    };

    // ① 走一次登录续期。刷新失败不直接判 Failed：可能只是上游没发新 token，
    //    积分仍可能已经由 identity 那次会话触发发放。
    let mut active_token = token.to_string();
    if !credential.refresh_token.is_empty() {
        match oauth::refresh(&client, endpoints::api_base(), &credential.refresh_token).await {
            Ok(fresh) => {
                active_token = fresh.access_token;
                agent2api_provider_sdk::log_info!("[Meoo] 签到：已用 refresh_token 完成一次登录续期");
            }
            Err(error) => {
                agent2api_provider_sdk::log_warn!("[Meoo] 签到续期失败：{error}");
            }
        }
    }
    // ② 会话 touch：服务端在「当日首次登录」这一刻发放积分。
    let _ = client::identity(&client, &active_token).await;

    // ③ 回读取证，容忍发放延迟（与 wpslingxi 的两轮回查同口径）。
    let mut delta: Option<f64> = None;
    let mut after: Option<f64> = None;
    let mut read_error: Option<String> = None;
    for attempt in 0..2u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(1_200)).await;
        }
        match client::points(&client, &active_token).await {
            Ok(points) => {
                let value = balance(&points);
                after = value;
                if let (Some(start), Some(end)) = (before, value) {
                    if end > start {
                        delta = Some(end - start);
                    }
                }
                if delta.is_some() {
                    break;
                }
                read_error = None;
            }
            Err(error) => read_error = Some(error),
        }
    }

    if let Some(granted) = delta {
        let credit = if granted >= endpoints::DAILY_LOGIN_GRANT - 1.0 {
            format!("{granted:.0} 积分（每日登录赠送）")
        } else {
            format!("{granted:.0} 积分")
        };
        return Ok(report(
            proto::CheckinOutcome::Claimed,
            Some(credit),
            "余额已实际增加，今日登录赠积分到账",
        ));
    }

    let entitlement = client::entitlement(&client, &active_token)
        .await
        .unwrap_or(Value::Null);
    let active = daily_quota(&entitlement).unwrap_or(endpoints::DAILY_LOGIN_GRANT) > 0.0;
    if !active {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            None,
            "该账号没有每日登录赠积分权益",
        ));
    }
    if let Some(error) = read_error {
        return Ok(report(
            proto::CheckinOutcome::Failed,
            None,
            format!("无法确认积分到账：{error}；请稍后重试"),
        ));
    }
    if before.is_none() || after.is_none() {
        return Ok(report(
            proto::CheckinOutcome::Failed,
            None,
            "上游没有返回可比较的积分余额，无法确认到账；请稍后重试",
        ));
    }
    Ok(report(
        proto::CheckinOutcome::Done,
        None,
        "余额没有变化：今日积分已领过，或上游尚未发放（Meoo 无独立签到接口）",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_cli_points_shape_and_string_numbers() {
        let points = json!({"account": {"availableBalance": "12000", "totalGranted": 20000, "totalConsumed": 8000}});
        assert_eq!(balance(&points), Some(12_000.0));
    }

    #[test]
    fn reads_alternative_balance_shapes() {
        assert_eq!(balance(&json!({"availableBalance": 5})), Some(5.0));
        assert_eq!(balance(&json!({"account": {"totalRemaining": 7}})), Some(7.0));
        assert_eq!(balance(&json!({})), None);
    }

    #[test]
    fn daily_quota_reads_benefit_or_defaults() {
        let entitlement = json!({"benefits": [{"typeCode": "daily_login", "quota": "2000"}]});
        assert_eq!(daily_quota(&entitlement), Some(2_000.0));
        assert_eq!(daily_quota(&json!({})), None);
    }

    #[test]
    fn number_rejects_bad_values() {
        assert_eq!(number(Some(&json!("1,234"))), Some(1_234.0));
        assert_eq!(number(Some(&json!(true))), None);
        assert_eq!(number(None), None);
    }
}
