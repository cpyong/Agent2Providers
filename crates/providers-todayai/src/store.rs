//! `secret` 的部分字段更新。
//!
//! ── 真正的账号库在网关侧 ────────────────────────────────────
//! 插件通过 `PluginCtx::store_get` / `store_update` 读写账号；`store_update`
//! 是**比较再写**（`expect` 与当前值不一致时拒绝），所以这里只需要准备一份
//! 「原 secret + 改几个键」的副本，不必关心事务。
//!
//! 与合集包 `crates/providers/src/store.rs` 的 `with_fields` 同源；本包只用
//! 到这一个函数 —— 换发 JWT 之后写回 `accessToken` / `tokenExpiresAt`。

use serde_json::Value;

/// 复制一份 secret，并把 `fields` 里的键覆盖进去。
///
/// `secret` 不是对象时原样返回（调用方的 `from_secret` 已经会报错，这里
/// 不做二次判定）。
pub fn with_fields(secret: &Value, fields: &[(&str, Value)]) -> Value {
    let mut next = secret.clone();
    let Some(object) = next.as_object_mut() else {
        return next;
    };
    for (key, value) in fields {
        object.insert((*key).to_string(), value.clone());
    }
    next
}
