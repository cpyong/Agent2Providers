//! 用系统默认浏览器打开 OAuth 授权页。
//!
//! ── 为什么不能靠页面 `window.open` ────────────────────────────
//! 插件自有页面托管在网关的 webview / iframe 里。宿主的
//! `setWindowOpenHandler`（或 iframe sandbox 没给 `allow-popups`）会把
//! `window.open` 与 `<a target="_blank">` 一起拦掉：点按钮后 flow 照常启动、
//! 轮询也一直在跑，用户却看不到任何窗口，只对着「等待授权」发呆。
//! 插件进程是本机桌面会话里的子进程，由它触发系统默认浏览器（ShellExecute
//! 语义）不受宿主页面策略影响。
//!
//! ── 为什么不用 `cmd /C start` ─────────────────────────────────
//! OAuth 的 `redirect_uri` 是百分号编码的（`http%3A%2F%2F…`），经过 cmd 会
//! 被环境变量展开吃掉，`&` 也会被当命令分隔符。这里把 URL 作为单个
//! `CreateProcess` 参数传递，不经过任何 shell。Windows 选 `rundll32.exe`
//! 的 `FileProtocolHandler`：它是 GUI 子系统程序，服务化启动的插件也不会
//! 因此弹黑框。
//!
//! ── 无桌面 / 自动化测试 ──────────────────────────────────────
//! 设 `MEOO_OPEN_BROWSER=0` 可关闭自动开窗（探针跑假上游时不能真弹浏览器）。
//! 关掉后页面会显示完整授权地址与一键复制，用户仍可手动完成授权。

use std::process::{Command, Stdio};

pub const DISABLE_ENV: &str = "MEOO_OPEN_BROWSER";

fn disabled(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("0"))
}

pub fn auto_open_enabled() -> bool {
    !disabled(std::env::var(DISABLE_ENV).ok().as_deref())
}

/// 只放行 http(s)，且不含空白 / 控制字符 / 引号。参数不经过 shell，但拼错
/// 的 URL 也别拿去调系统关联程序。
fn safe_url(url: &str) -> bool {
    let has_scheme = url.starts_with("https://") || url.starts_with("http://");
    has_scheme
        && url.len() > 12
        && !url.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '`')
        })
}

#[cfg(target_os = "windows")]
fn platform(url: &str) -> (&'static str, Vec<String>) {
    (
        "rundll32.exe",
        vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
    )
}

#[cfg(target_os = "macos")]
fn platform(url: &str) -> (&'static str, Vec<String>) {
    ("open", vec![url.to_string()])
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform(url: &str) -> (&'static str, Vec<String>) {
    ("xdg-open", vec![url.to_string()])
}

/// 命令构造抽成纯函数，方便单测覆盖三平台与 URL 校验。
pub fn command(url: &str) -> Result<(&'static str, Vec<String>), String> {
    if !safe_url(url) {
        return Err("拒绝打开非 http(s) 或含控制字符的地址".to_string());
    }
    Ok(platform(url))
}

/// 只 `spawn` 不 `wait`：关联程序自己会退出，浏览器进程独立。成功仅表示
/// 「已请求系统打开」，不保证用户真的看见了窗口 —— 页面仍保留手动兜底。
pub fn open(url: &str) -> Result<(), String> {
    if !auto_open_enabled() {
        return Err(format!("已按 {DISABLE_ENV}=0 关闭自动开窗"));
    }
    let (program, args) = command(url)?;
    Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|error| format!("无法启动系统默认浏览器（{program}）：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_switch_only_matches_zero() {
        assert!(!disabled(None));
        assert!(!disabled(Some("1")));
        assert!(!disabled(Some("")));
        assert!(disabled(Some("0")));
        assert!(disabled(Some(" 0 ")));
    }

    #[test]
    fn rejects_unsafe_urls() {
        for url in [
            "",
            "https://",
            "javascript:alert(1)",
            "file:///C:/Windows/system.ini",
            "https://meoo.com/a b",
            "https://meoo.com/a\"b",
            "https://meoo.com/a\nHost: evil",
        ] {
            assert!(command(url).is_err(), "不该放行：{url}");
        }
    }

    #[test]
    fn keeps_percent_encoded_oauth_query_intact() -> Result<(), String> {
        let url = "https://meoo.com/oauth/authorize?client_id=meoo_app_x&state=a%2Fb&redirect_uri=http%3A%2F%2F127.0.0.1%3A5182%2Foauth%2Fcallback";
        let (program, args) = command(url)?;
        assert!(!program.is_empty());
        assert_eq!(args.last().map(String::as_str), Some(url));
        #[cfg(target_os = "windows")]
        {
            assert_eq!(program, "rundll32.exe");
            assert_eq!(args.first().map(String::as_str), Some("url.dll,FileProtocolHandler"));
        }
        Ok(())
    }
}
