//! 智点余额与每日签到（任务中心 `daily_check_in`）。
//!
//! ── 签到协议（WPS 官方社区教程 + 公开实现双重取证）─────────────
//! ```text
//!   GET  /api/public/v1/tasks
//!        → data.tasks[]，找 task_key == "daily_check_in"
//!        → status == "claimed" 表示今日已签；extra.consecutive_days /
//!          extra.today_reward 给连续天数与当日奖励
//!   POST /api/public/v1/tasks/daily_check_in/claim?date=YYYY-MM-DD
//!        body {}（date 按 UTC+8 换日）
//!        → {result:"ok", data.extra.check_in_days:[{date,status,reward}]}
//! ```
//!
//! ── 为什么绝不能只看 HTTP 200 / result=ok ───────────────────
//! 上游对**重复领取**回 200 假成功（`result=ok` 而余额不动）。判「这次真领
//! 到了」只有两条硬证据：① 领前/领后 `total_balance` 的差；② 回查任务状态
//! 翻成 `claimed`（或 claim 响应里当天的 `check_in_days[].status`）。两条都
//! 没有就报 `Failed` 让定时链明天再试，**不能**点亮「已领」。
//! 这与 Trae 的「同设备号回查未翻转不算成功」是同一条纪律。
//!
//! ── 一轮发几发 ─────────────────────────────────────────────
//! 余额 1 + 任务 1 + （未签时）领取 1 + 余额 1 + 任务 1 = 5 发；两条证据都没
//! 出现时再补**一轮**（等 1.2 秒，余额 + 任务各 1）→ 最多 7 发。
//! 「今日已签」在任务状态那一步就短路（只发 2 发）。不做指数退避重试：
//! 上游的风控画像看的是请求节奏，不是我们有没有重试成功。

use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{client, credentials, ID};
use crate::http;

/// 智点余额读数（账号页的「积分」按钮走这条）。
///
/// ── `available` 这个键不能省 ─────────────────────────────────
/// 网关把 usage 报告折成账号卡上的「剩余额度」时，只读顶层的
/// `totalLeft`（存在则优先）或 `available`，**完全不读 `balance`**。
/// 只给 `balance` 的表现很有迷惑性：上游请求 200、日志没有报错，但
/// `account_usage_records.remaining` 落成 NULL，界面就是「无法获取余额」。
/// 所以报告要按网关实际的形状给全：
///   · `available` + `unit`        → 账号卡主额度（`remaining`）
///   · `wallets[]`                 → 钱包明细（`balance` / `displayName`）
///   · `quotaSources[]`            → 额度来源（含 `id:"account"` 主额度项）
/// `balance` / `credits` / `plan` 一并保留，兼容旧消费方。
pub async fn usage(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let (credential, proxy) = credentials::load(ctx, account).await?;
    let http_client = http::client(proxy.as_ref(), &ctx.env.proxy);
    let data = client::balance(&http_client, &credential)
        .await
        .map_err(auth_hint)?;
    Ok(usage_document(&data))
}

/// 由余额信封构造报告（纯函数，方便直接回归网关契约）。
fn usage_document(data: &Value) -> Value {
    // 赠送 / 购买智点都是「按批次」给的，每批有自己的余额与到期时刻。
    let mut batches: Vec<Batch> = Vec::new();
    for (key, label) in [
        ("bonus_credits", "赠送智点"),
        ("purchased_credits", "购买智点"),
    ] {
        if let Some(items) = data.get(key).and_then(Value::as_array) {
            for item in items {
                let Some(amount) = number(item.get("balance")) else {
                    continue;
                };
                if amount <= 0.0 {
                    continue;
                }
                batches.push(Batch {
                    name: label.to_string(),
                    wallet_type: key.to_string(),
                    amount,
                    expire: expire_ms(item.get("expire_time")),
                });
            }
        }
    }

    let remaining = balance_number(&data)
        .unwrap_or_else(|| batches.iter().map(|batch| batch.amount).sum());

    // 旧的 credits 视图（逐批次 remain / total / expireAt）。
    let credits: Vec<Value> = batches
        .iter()
        .map(|batch| {
            let mut row = json!({
                "name": batch.name,
                "total": batch.amount,
                "used": 0,
                "remain": batch.amount,
                "unit": "智点",
            });
            insert_expire(&mut row, batch.expire, "expireAt");
            row
        })
        .collect();

    // 账号卡的钱包列表：available 缺席时网关也会从这里找第一个 balance。
    let wallets: Vec<Value> = batches
        .iter()
        .map(|batch| {
            let mut row = json!({
                "balance": batch.amount,
                "balanceView": format!("剩 {}", credit_label(batch.amount)),
                "displayName": batch.name,
                "type": batch.wallet_type,
            });
            insert_expire(&mut row, batch.expire, "expireAt");
            row
        })
        .collect();

    // 额度来源：第一项必须是 id:"account"，网关靠它取 unit 与主余额。
    let mut sources = Vec::with_capacity(batches.len() + 1);
    sources.push(json!({
        "id": "account",
        "label": "账号主额度",
        "provider": ID,
        "remaining": remaining,
        "total": null,
        "used": null,
        "unit": "智点",
        "unlimited": false,
    }));
    for (index, batch) in batches.iter().enumerate() {
        let mut row = json!({
            "id": format!("credit:{}:{index}", batch.name),
            "label": batch.name,
            "provider": ID,
            "remaining": batch.amount,
            "total": batch.amount,
            "used": 0,
            "unit": "智点",
            "unlimited": false,
        });
        insert_expire(&mut row, batch.expire, "endsAt");
        sources.push(row);
    }

    json!({
        "provider": ID,
        "unit": "智点",
        "plan": "WPS 灵犀智点",
        // ↓ 网关只认这两个键之一；balance 是给老消费方留的别名
        "available": remaining,
        "balance": remaining,
        "availableView": format!("可用 {}", credit_label(remaining)),
        "credits": credits,
        "wallets": wallets,
        "quotaSources": sources,
    })
}

/// 一批智点（赠送 / 购买的单个到期批次）。
struct Batch {
    name: String,
    wallet_type: String,
    amount: f64,
    expire: Option<i64>,
}

/// 有到期时刻才写字段，避免给界面塞一堆 null。
fn insert_expire(row: &mut Value, expire: Option<i64>, key: &str) {
    if let (Some(expire), Some(object)) = (expire, row.as_object_mut()) {
        object.insert(key.to_string(), json!(expire));
    }
}

/// 只读签到状态（界面「今日是否已签 / 连续天数」）。
pub async fn status(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let (credential, proxy) = credentials::load(ctx, account).await?;
    let http_client = http::client(proxy.as_ref(), &ctx.env.proxy);
    let tasks = client::tasks(&http_client, &credential)
        .await
        .map_err(auth_hint)?;
    let Some(daily) = find_daily(&tasks) else {
        return Ok(json!({
            "active": false,
            "todayCheckedIn": false,
            "activityName": "每日签到（智点）",
        }));
    };
    let extra = daily.get("extra").unwrap_or(&Value::Null);
    Ok(json!({
        "active": true,
        "todayCheckedIn": daily.get("status").and_then(Value::as_str) == Some("claimed"),
        "streakDays": number(extra.get("consecutive_days")),
        "todayCredit": number(extra.get("today_reward")),
        "activityName": "每日签到（智点）",
    }))
}

/// 每日签到：状态短路 → 领取 → 余额差 + 状态回查双重确认。
pub async fn checkin(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, String> {
    let (credential, proxy) = credentials::load(ctx, account).await?;
    let http_client = http::client(proxy.as_ref(), &ctx.env.proxy);

    // 领前余额。失败不拦签到（余额接口与任务接口是两条独立链路），只让
    // 「余额差」这条证据缺席。
    let before = read_balance(&http_client, &credential).await;
    let tasks = match client::tasks(&http_client, &credential).await {
        Ok(tasks) => tasks,
        Err(error) => return Err(auth_hint(error)),
    };
    let Some(daily) = find_daily(&tasks) else {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            None,
            None,
            "上游任务中心里没有每日签到活动（账号可能不参与该档）",
        ));
    };
    if daily.get("status").and_then(Value::as_str) == Some("claimed") {
        let extra = daily.get("extra").unwrap_or(&Value::Null);
        return Ok(report(
            proto::CheckinOutcome::Done,
            reward_of(extra.get("today_reward")).map(credit_label),
            streak_of(extra),
            "今日已签到",
        ));
    }

    let date = claim_date(daily);
    let claim = match client::claim(&http_client, &credential, &date).await {
        Ok(payload) => payload,
        Err(error) => {
            return Ok(report(
                proto::CheckinOutcome::Failed,
                None,
                None,
                &auth_hint(error),
            ));
        }
    };
    // result 非 ok 是明确的拒绝，不再往下查余额（避免把失败洗成幂等成功）。
    let result = claim
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !result.is_empty() && !result.eq_ignore_ascii_case("ok") {
        let hint = client::text_of(&claim);
        return Ok(report(
            proto::CheckinOutcome::Failed,
            None,
            None,
            &if hint.is_empty() {
                format!("上游拒绝领取（result={result}）")
            } else {
                hint
            },
        ));
    }

    // ── 领取后取证 ────────────────────────────────────────────
    // 两条硬证据：余额差、任务状态回查翻转。上游状态是最终一致的，偶尔会晚
    // 一两秒才翻；只读一次就报失败，会把「其实已经签到成功」记成失败（要等到
    // 第二天才被任务状态纠正）。所以只多做**一轮**短等待复核，不做指数退避：
    // 上游风控看的是请求节奏，不是我们重试到成功为止。
    let mut delta: Option<f64> = None;
    let mut status_flipped = false;
    let mut after_daily: Option<Value> = None;
    for attempt in 0..2u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(1_200)).await;
        }
        let after = read_balance(&http_client, &credential).await;
        delta = match (before, after) {
            (Some(before), Some(after)) if after > before => Some(after - before),
            _ => None,
        };
        let after_tasks = client::tasks(&http_client, &credential)
            .await
            .unwrap_or_default();
        let daily_after = find_daily(&after_tasks).cloned();
        status_flipped = daily_after
            .as_ref()
            .is_some_and(|task| task.get("status").and_then(Value::as_str) == Some("claimed"));
        after_daily = daily_after;
        if delta.is_some() || status_flipped {
            break;
        }
    }
    let day_flipped = claimed_today(&claim, &date);
    let confirmed = status_flipped || day_flipped;
    let reward = delta
        .or_else(|| claim_reward(&claim, &date))
        .or_else(|| {
            after_daily.as_ref().and_then(|task| {
                reward_of(task.get("extra").and_then(|extra| extra.get("today_reward")))
            })
        })
        .or_else(|| reward_of(daily.get("extra").and_then(|extra| extra.get("today_reward"))));
    let streak = after_daily
        .as_ref()
        .and_then(|task| streak_of(task.get("extra").unwrap_or(&Value::Null)))
        .or_else(|| streak_of(daily.get("extra").unwrap_or(&Value::Null)));

    if (confirmed && reward.is_some()) || delta.is_some() {
        return Ok(report(
            proto::CheckinOutcome::Claimed,
            reward.map(credit_label),
            streak,
            &match delta {
                Some(amount) => format!("签到成功，智点余额 +{}", format_amount(amount)),
                None => "签到成功（上游任务状态已翻转，未给出奖励数额）".to_string(),
            },
        ));
    }
    if confirmed {
        // 状态已翻但余额没动：上游幂等（多半是官方客户端同日先签了）。
        return Ok(report(
            proto::CheckinOutcome::Done,
            reward.map(credit_label),
            streak,
            "今日已签到（上游幂等，余额未变化）",
        ));
    }
    Ok(report(
        proto::CheckinOutcome::Failed,
        None,
        streak,
        "领取请求已发出，但任务状态未翻转且余额未增加，无法确认到账；请稍后重试",
    ))
}

/// 把「登录态失效」的错误翻成人话（其余原样保留）。
fn auth_hint(error: String) -> String {
    if error.starts_with(client::AUTH_ERROR_PREFIX) {
        "WPS 灵犀登录态已失效，请重新粘贴 wps_sid Cookie（上游没有续期接口）".to_string()
    } else {
        error
    }
}

fn report(
    outcome: proto::CheckinOutcome,
    credit: Option<String>,
    streak: Option<u32>,
    message: &str,
) -> proto::CheckinReport {
    proto::CheckinReport {
        outcome,
        credit,
        streak,
        message: Some(message.to_string()),
    }
}

fn find_daily(tasks: &[Value]) -> Option<&Value> {
    tasks.iter().find(|task| {
        task.get("task_key").and_then(Value::as_str) == Some("daily_check_in")
    })
}

async fn read_balance(
    http_client: &reqwest::Client,
    credential: &credentials::Credential,
) -> Option<f64> {
    let data = client::balance(http_client, credential).await.ok()?;
    balance_number(&data)
}

/// `total_balance` 优先；缺失时把赠送/购买批次加起来（另一个公开实现报的
/// 形状没有 `total_balance`，两种都要认）。
fn balance_number(data: &Value) -> Option<f64> {
    if let Some(total) = number(data.get("total_balance")) {
        return Some(total);
    }
    let mut sum = 0.0;
    let mut seen = false;
    for key in ["bonus_credits", "purchased_credits"] {
        if let Some(items) = data.get(key).and_then(Value::as_array) {
            for item in items {
                if let Some(amount) = number(item.get("balance")) {
                    sum += amount;
                    seen = true;
                }
            }
        }
    }
    if seen {
        return Some(sum);
    }
    number(
        data.get("bonus_summary")
            .and_then(|summary| summary.get("total_value")),
    )
}

/// 宽松取数（上游有时把数字写成字符串）。
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

fn reward_of(value: Option<&Value>) -> Option<f64> {
    number(value).filter(|amount| *amount > 0.0)
}

fn streak_of(extra: &Value) -> Option<u32> {
    number(extra.get("consecutive_days"))
        .filter(|value| *value >= 0.0)
        .map(|value| value as u32)
}

fn credit_label(amount: f64) -> String {
    format!("{} 智点", format_amount(amount))
}

fn format_amount(amount: f64) -> String {
    if (amount - amount.round()).abs() < 0.000_001 {
        format!("{}", amount.round() as i64)
    } else {
        format!("{amount:.2}")
    }
}

/// `expire_time` → Unix 毫秒时间戳。
///
/// 上游实际给的是 ISO 8601 字符串（`"2026-12-12T11:02:43+08:00"`），
/// 早期实现只按数字读，结果是 `expireAt` 全丢、账号卡的到期时间一片空白。
/// 这里保留「秒 / 毫秒数字」两种老形状，再补一段无第三方依赖的 ISO 解析
/// （含 `Z` 与 `±HH:MM` 偏移）。
fn expire_ms(value: Option<&Value>) -> Option<i64> {
    let raw = value?;
    match raw {
        Value::Number(_) => epoch_scaled(number(Some(raw))?),
        Value::String(text) => {
            let trimmed = text.trim();
            if let Ok(numeric) = trimmed.parse::<f64>() {
                return epoch_scaled(numeric);
            }
            iso8601_ms(trimmed)
        }
        _ => None,
    }
}

/// 数字 > 1e11 视为毫秒，否则按秒换算。
fn epoch_scaled(raw: f64) -> Option<i64> {
    if raw <= 0.0 || !raw.is_finite() {
        return None;
    }
    Some(if raw > 100_000_000_000.0 {
        raw as i64
    } else {
        (raw * 1000.0) as i64
    })
}

/// 解析 `YYYY-MM-DD[T ]HH:MM:SS[.sss][Z|±HH:MM]`（无时区时按 UTC）。
fn iso8601_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || (bytes.get(10) != Some(&b'T') && bytes.get(10) != Some(&b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let mut epoch = days_from_civil(year, month, day) * 86_400
        + hour * 3_600
        + minute * 60
        + second;

    // 尾部：可选小数秒 + 时区（Z / +08:00 / -0530 / +08）
    let mut tail = text.get(19..)?.trim();
    if let Some(fraction) = tail.strip_prefix('.') {
        let cut = fraction
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(fraction.len());
        tail = &fraction[cut..];
    }
    if !tail.is_empty() && tail != "Z" && tail != "z" {
        let (sign, rest) = match tail.as_bytes().first()? {
            b'+' => (1i64, &tail[1..]),
            b'-' => (-1i64, &tail[1..]),
            _ => return None,
        };
        let digits: String = rest.chars().filter(|ch| ch.is_ascii_digit()).collect();
        let (offset_hour, offset_minute) = match digits.len() {
            2 => (digits.parse::<i64>().ok()?, 0),
            4 => (
                digits.get(..2)?.parse::<i64>().ok()?,
                digits.get(2..4)?.parse::<i64>().ok()?,
            ),
            _ => return None,
        };
        epoch -= sign * (offset_hour * 3_600 + offset_minute * 60);
    }
    Some(epoch * 1000)
}

/// 公历日期 → 1970-01-01 的天数（Howard Hinnant 的 civil_from_days 逆运算）。
/// 不引 chrono：只需要把上游的到期时刻换成毫秒整数，不值得多一个依赖。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let shifted = year - if month <= 2 { 1 } else { 0 };
    let era = if shifted >= 0 { shifted } else { shifted - 399 } / 400;
    let year_of_era = shifted - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 签到窗口日期：任务里给了就用，否则按 UTC+8 的今天（上游按北京换日）。
fn claim_date(daily: &Value) -> String {
    let extra = daily.get("extra").unwrap_or(&Value::Null);
    for key in ["claim_date", "date", "today"] {
        if let Some(value) = extra.get(key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    beijing_today()
}

/// UTC+8 的 `YYYY-MM-DD`（不引 chrono：天数 → 公历的换算只需要几十行）。
fn beijing_today() -> String {
    let seconds = http::now_ms() / 1000 + 8 * 3600;
    let days = seconds.div_euclid(86_400);
    // Howard Hinnant 的 civil_from_days（days 自 1970-01-01 起）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}")
}

/// claim 响应里当天那条 `check_in_days[]` 是否已翻成 `claimed`。
fn claimed_today(claim: &Value, date: &str) -> bool {
    claim
        .get("data")
        .and_then(|data| data.get("extra"))
        .and_then(|extra| extra.get("check_in_days"))
        .and_then(Value::as_array)
        .and_then(|days| {
            days.iter().find(|item| {
                item.get("date").and_then(Value::as_str) == Some(date)
            })
        })
        .is_some_and(|item| item.get("status").and_then(Value::as_str) == Some("claimed"))
}

/// 从 claim 响应里取奖励数额（余额差缺席时才用）。
fn claim_reward(claim: &Value, date: &str) -> Option<f64> {
    let data = claim.get("data").unwrap_or(&Value::Null);
    let extra = data.get("extra").unwrap_or(&Value::Null);
    if let Some(amount) = reward_of(extra.get("today_reward")) {
        return Some(amount);
    }
    if let Some(amount) = reward_of(data.get("reward_amount")) {
        return Some(amount);
    }
    extra
        .get("check_in_days")
        .and_then(Value::as_array)
        .and_then(|days| {
            days.iter().find(|item| {
                item.get("date").and_then(Value::as_str) == Some(date)
            })
        })
        .and_then(|item| reward_of(item.get("reward")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beijing_date_is_utc_plus_eight() {
        let date = beijing_today();
        assert_eq!(date.len(), 10, "日期应为 YYYY-MM-DD：{date}");
        assert_eq!(date.as_bytes()[4], b'-');
        assert_eq!(date.as_bytes()[7], b'-');
        let year: i32 = date[..4].parse().unwrap();
        assert!((2020..2100).contains(&year));
    }

    #[test]
    fn balance_reads_both_upstream_shapes() {
        assert_eq!(
            balance_number(&json!({ "total_balance": 42.5 })),
            Some(42.5)
        );
        let without_total = json!({
            "bonus_credits": [{ "balance": "10", "expire_time": 1_800_000_000 }],
            "purchased_credits": [{ "balance": 2.5 }],
        });
        assert_eq!(balance_number(&without_total), Some(12.5));
        assert_eq!(
            expire_ms(without_total["bonus_credits"][0].get("expire_time")),
            Some(1_800_000_000_000)
        );
    }

    #[test]
    fn status_flip_and_reward_are_read_from_claim_response() {
        let date = beijing_today();
        let claim = json!({
            "result": "ok",
            "data": {
                "extra": {
                    "today_reward": 5,
                    "check_in_days": [{ "date": date, "status": "claimed", "reward": 5 }],
                },
            },
        });
        assert!(claimed_today(&claim, &date));
        assert_eq!(claim_reward(&claim, &date), Some(5.0));
        let daily = json!({
            "task_key": "daily_check_in",
            "status": "unclaimed",
            "extra": { "claim_date": date, "consecutive_days": 29 },
        });
        assert_eq!(claim_date(&daily), date);
        assert_eq!(streak_of(daily.get("extra").unwrap()), Some(29));
        assert_eq!(find_daily(&[daily]).unwrap()["task_key"], "daily_check_in");
    }

    #[test]
    fn plain_http_200_without_evidence_is_not_claimed() {
        // 重复领取：result=ok，但既没有余额差、也没有状态翻转 —— 判定要看
        // checkin() 里的 confirmed / delta，这里守住两个取证函数的口径。
        let claim = json!({ "result": "ok", "data": {} });
        let date = beijing_today();
        assert!(!claimed_today(&claim, &date));
        assert_eq!(claim_reward(&claim, &date), None);
        assert_eq!(credit_label(3.0), "3 智点");
    }

    #[test]
    fn usage_report_has_gateway_available_key() {
        // 真实上游形状（2026-11-12 取证）：total_balance + 三批赠送智点，
        // expire_time 是 ISO 8601 字符串而非秒数。
        let data = json!({
            "enabled": true,
            "bonus_credits": [
                { "balance": 800, "expire_time": "2026-12-12T11:02:43+08:00" },
                { "balance": 100, "expire_time": "2026-12-12T11:02:58+08:00" },
                { "balance": 100, "expire_time": "2026-12-13T08:06:32+08:00" },
            ],
            "purchased_credits": [],
            "total_balance": 1000,
        });
        let report = usage_document(&data);
        // 网关只把 totalLeft / available 折成 remaining，缺了就是「无法获取余额」
        assert_eq!(report["available"], 1000.0);
        assert_eq!(report["balance"], 1000.0);
        assert_eq!(report["unit"], "智点");
        assert_eq!(report["credits"].as_array().map(Vec::len), Some(3));
        assert_eq!(report["wallets"].as_array().map(Vec::len), Some(3));
        let sources = report["quotaSources"].as_array().expect("quotaSources");
        assert_eq!(sources[0]["id"], "account");
        assert_eq!(sources[0]["remaining"], 1000.0);
        assert_eq!(sources.len(), 4);
        // 到期时刻三处都要带上（上游给的是 ISO，不是数字）
        assert_eq!(report["credits"][0]["expireAt"], 1_797_044_563_000i64);
        assert_eq!(report["wallets"][0]["expireAt"], 1_797_044_563_000i64);
        assert_eq!(sources[1]["endsAt"], 1_797_044_563_000i64);
    }

    #[test]
    fn iso_expiry_handles_offsets_and_legacy_numbers() {
        assert_eq!(
            iso8601_ms("2026-12-12T11:02:43+08:00"),
            Some(1_797_044_563_000)
        );
        assert_eq!(
            iso8601_ms("2026-12-12T03:02:43Z"),
            Some(1_797_044_563_000)
        );
        assert_eq!(iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso8601_ms("2026-12-12 03:02:43.500+00:00"),
            Some(1_797_044_563_000)
        );
        assert_eq!(
            iso8601_ms("2026-12-12T03:02:43-05:30"),
            Some(1_797_044_563_000 + 19_800_000)
        );
        assert_eq!(iso8601_ms("not a date"), None);
        // 公开实现里的秒 / 毫秒数字形状仍要认
        assert_eq!(
            expire_ms(Some(&json!(1_800_000_000))),
            Some(1_800_000_000_000)
        );
        assert_eq!(
            expire_ms(Some(&json!("1800000000"))),
            Some(1_800_000_000_000)
        );
        assert_eq!(
            expire_ms(Some(&json!(1_800_000_000_000i64))),
            Some(1_800_000_000_000i64)
        );
    }

    #[test]
    fn usage_without_total_balance_sums_batches() {
        let data = json!({
            "bonus_credits": [{ "balance": "10" }],
            "purchased_credits": [{ "balance": 2.5 }],
        });
        let report = usage_document(&data);
        assert_eq!(report["available"], 12.5);
        assert_eq!(report["quotaSources"][0]["remaining"], 12.5);
        assert_eq!(report["wallets"][0]["balance"], 10.0);
    }
}
