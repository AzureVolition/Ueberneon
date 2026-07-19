// web_fetch 工具 —— 从 URL 获取内容。
//
// 支持 HTTP/HTTPS，自动将 HTML 转为纯文本。
// 内置 SSRF 防护：拒绝私有 IP、回环地址和链路本地地址。

use crate::agent::{Tool, ToolContext, ToolResult};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use crate::agent::{AgentMode, ActionMode, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// web_fetch — 从 URL 获取文本内容。
///
/// HTML 页面自动转为纯文本（去掉 script/style tag，
/// 插入 Markdown 风格标题和列表）。JSON/纯文本原样返回。
#[derive(ToolMetaImpl)]
pub struct WebFetch {
    schema: Value,
    read_only: bool,
}

const WEB_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const WEB_FETCH_MAX_READ: u64 = 1_048_576; // 1 MiB
const USER_AGENT: &str = "racpagent-web-fetch/1.0";

impl WebFetch {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute URL beginning with http:// or https://"
                    }
                },
                "required": ["url"]
            }),
            read_only: true,
        }
    }

    /// 检查 IP 是否为私有/回环/链路本地地址（SSRF 防护）。
    fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_private()           // 10.x, 172.16-31.x, 192.168.x
                    || v4.is_loopback()      // 127.x
                    || v4.is_link_local()    // 169.254.x
                    || v4.is_unspecified()   // 0.0.0.0
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()           // ::1
                    || v6.is_unspecified()    // ::
                    || Self::is_ipv6_unique_local(v6)   // fc00::/7
                    || Self::is_ipv6_link_local(v6)     // fe80::/10
            }
        }
    }

    /// 检查 IPv6 是否为 unique-local (fc00::/7)。
    fn is_ipv6_unique_local(ip: &std::net::Ipv6Addr) -> bool {
        ip.octets()[0] & 0xfe == 0xfc
    }

    /// 检查 IPv6 是否为 link-local (fe80::/10)。
    fn is_ipv6_link_local(ip: &std::net::Ipv6Addr) -> bool {
        ip.octets()[0] == 0xfe && (ip.octets()[1] & 0xc0) == 0x80
    }

    /// 解析域名并检查是否指向被阻止的 IP。
    async fn check_ssrf(host: &str) -> Result<(), String> {
        // 先尝试解析为 IP 字面量
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            if Self::is_blocked_ip(&ip) {
                return Err(format!(
                    "SSRF blocked: '{}' resolves to a private/loopback address ({})",
                    host, ip
                ));
            }
            return Ok(());
        }

        // 域名：进行 DNS 解析
        let addr_str = format!("{}:0", host);
        match tokio::net::lookup_host(&addr_str).await {
            Ok(addrs) => {
                for addr in addrs {
                    if Self::is_blocked_ip(&addr.ip()) {
                        return Err(format!(
                            "SSRF blocked: '{}' resolves to a blocked address ({})",
                            host, addr.ip()
                        ));
                    }
                }
                Ok(())
            }
            Err(_) => {
                // DNS 解析失败——保守处理：阻止
                Err(format!(
                    "SSRF blocked: unable to resolve '{}'",
                    host
                ))
            }
        }
    }

    /// 简单的 HTML 转纯文本。
    fn html_to_text(html: &str) -> String {
        let mut result = String::new();
        let bytes = html.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        let mut in_tag = false;
        let mut in_script = false;
        let mut in_style = false;
        let mut tag_name = String::new();

        while i < len {
            if !in_tag {
                if bytes[i] == b'<' {
                    in_tag = true;
                    tag_name.clear();
                    i += 1;
                    continue;
                }
                // 在文本中，解码 HTML 实体
                if i + 1 < len && bytes[i] == b'&' {
                    // &quot; -> "
                    if i + 5 < len && &bytes[i..i+6] == b"&quot;" {
                        result.push('"');
                        i += 6;
                        continue;
                    }
                    // &#34; -> "
                    if i + 5 < len && &bytes[i..i+6] == b"&#34;" {
                        result.push('"');
                        i += 6;
                        continue;
                    }
                    // &lt; -> <
                    if i + 3 < len && &bytes[i..i+4] == b"&lt;" {
                        result.push('<');
                        i += 4;
                        continue;
                    }
                    if i + 3 < len && &bytes[i..i+4] == b"&gt;" {
                        result.push('>');
                        i += 4;
                        continue;
                    }
                    if i + 4 < len && &bytes[i..i+5] == b"&amp;" {
                        result.push('&');
                        i += 5;
                        continue;
                    }
                    if i + 1 < len && bytes[i] == b'&' && bytes[i+1] == b'#' {
                        // 跳过 &#...; 实体
                        let mut j = i + 2;
                        while j < len && bytes[j] != b';' {
                            j += 1;
                        }
                        if j < len {
                            // 解码 &#10; 等
                            let entity = &html[i+2..j];
                            if let Ok(code) = entity.parse::<u32>() {
                                if let Some(ch) = char::from_u32(code) {
                                    result.push(ch);
                                }
                            }
                            i = j + 1;
                            continue;
                        }
                    }
                }
                result.push(bytes[i] as char);
                i += 1;
            } else {
                // 在标签内部
                if bytes[i] == b'>' {
                    in_tag = false;
                    let tag_lower = tag_name.to_lowercase();
                    let tag_base = tag_lower.split_whitespace().next().unwrap_or("");

                    if tag_base == "script" {
                        in_script = false;
                    } else if tag_base == "style" {
                        in_style = false;
                    }

                    // 块级元素后加换行
                    if matches!(
                        tag_base,
                        "p" | "div" | "br" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                            | "li" | "tr" | "blockquote" | "pre" | "hr"
                    ) {
                        result.push('\n');
                    }
                    // 标题加 # 前缀
                    if let Some(n) = tag_base.strip_prefix('h') {
                        if let Ok(level) = n.parse::<usize>() {
                            for _ in 0..level {
                                result.push('#');
                            }
                            result.push(' ');
                        }
                    }
                    // 列表项加 - 前缀
                    if tag_base == "li" {
                        result.push_str("- ");
                    }
                    // 引用加 > 前缀
                    if tag_base == "blockquote" {
                        result.push_str("> ");
                    }
                    // 代码块标记
                    if tag_base == "pre" || tag_base == "code" {
                        // 在文本中加 `` ` 标记不好实现，跳过
                    }

                    i += 1;
                    continue;
                }
                // 收集标签名（忽略属性）
                if tag_name.is_empty() && (bytes[i] == b'/' || bytes[i] == b'!') {
                    // 跳过 </ 和 <! 后的内容直到 >
                } else if !bytes[i].is_ascii_whitespace() {
                    tag_name.push(bytes[i] as char);
                } else if !tag_name.is_empty() {
                    // 到达属性部分，不再收集 tag 名但继续
                }
                i += 1;
            }
        }

        // 压缩多余空行
        let mut cleaned = String::new();
        let mut prev_newline = false;
        for ch in result.chars() {
            if ch == '\n' {
                if prev_newline {
                    continue;
                }
                prev_newline = true;
            } else {
                prev_newline = false;
            }
            cleaned.push(ch);
        }

        cleaned.trim().to_string()
    }

    /// 检查内容是否看起来像 HTML。
    fn looks_like_html(content: &[u8]) -> bool {
        let lower = content[..content.len().min(4096)].to_ascii_lowercase();
        let s = std::str::from_utf8(&lower).unwrap_or("");
        s.contains("<!doctype html") || s.contains("<html") || s.contains("<head")
    }
}

#[async_trait::async_trait]
impl Tool for WebFetch {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let url_str = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => u.trim(),
            _ => return Err("web_fetch: missing required argument 'url'".into()),
        };

        // URL 格式校验
        let url = match url::Url::parse(url_str) {
            Ok(u) => u,
            Err(e) => return Err(format!("web_fetch: invalid URL '{}': {}", url_str, e)),
        };

        // 只允许 http/https
        match url.scheme() {
            "http" | "https" => {}
            scheme => return Err(format!(
                "web_fetch: only http/https URLs are allowed, got '{}'", scheme
            )),
        }

        // SSRF 防护
        let host = match url.host_str() {
            Some(h) => h,
            None => return Err(format!("web_fetch: URL '{}' has no host", url_str)),
        };

        if let Err(e) = Self::check_ssrf(host).await {
            return Err(e);
        }

        // 构建 HTTP 客户端
        let client = match reqwest::Client::builder()
            .timeout(WEB_FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("web_fetch: failed to create HTTP client: {}", e)),
        };

        // 执行请求
        let resp = match client.get(url.as_str()).send().await {
            Ok(r) => r,
            Err(e) => {
                if e.is_timeout() {
                    return Err(format!(
                        "web_fetch: request timed out after {}s", WEB_FETCH_TIMEOUT.as_secs()
                    ));
                }
                if e.is_connect() {
                    return Err(format!(
                        "web_fetch: connection failed: {}", e
                    ));
                }
                return Err(format!("web_fetch: request failed: {}", e));
            }
        };

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // 读取响应体（上限 1 MiB）
        let body = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return Err(format!("web_fetch: failed to read response body: {}", e)),
        };

        if body.is_empty() {
            return Ok(ToolResult::ok(format!(
                "(empty body — status {})",
                status
            )));
        }

        let body_len = body.len();

        // 判断内容类型并转换
        let is_html = content_type.contains("text/html")
            || content_type.contains("application/xhtml")
            || Self::looks_like_html(&body);

        let text_content = if body_len > WEB_FETCH_MAX_READ as usize {
            // 截断
            let truncated = &body[..WEB_FETCH_MAX_READ as usize];
            let text = if is_html {
                Self::html_to_text(std::str::from_utf8(truncated).unwrap_or(""))
            } else {
                String::from_utf8_lossy(truncated).to_string()
            };
            format!("{}\n... (truncated at {} bytes)", text, WEB_FETCH_MAX_READ)
        } else {
            if is_html {
                Self::html_to_text(std::str::from_utf8(&body).unwrap_or(""))
            } else {
                String::from_utf8_lossy(&body).to_string()
            }
        };

        // 构建输出
        let content_type_short = if is_html {
            "text/html"
        } else if content_type.contains("application/json") {
            "application/json"
        } else {
            "text/plain"
        };

        let byte_str = if body_len >= WEB_FETCH_MAX_READ as usize {
            format!("{} (truncated at 1 MiB)", body_len)
        } else {
            body_len.to_string()
        };

        let header = format!(
            "status {} {} · {} · {} bytes\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            content_type_short,
            byte_str,
        );

        Ok(ToolResult::ok(format!("{}{}", header, text_content)))
    }
}


#[async_trait::async_trait]
impl CheckableTool for WebFetch {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::tool::ToolMeta;

    fn test_ctx() -> ToolContext {
        ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            agent_mode: Arc::new(Mutex::new(AgentMode::Ask)),
            progress: None,
        }
    }

    #[tokio::test]
    async fn missing_url() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({})).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("missing"));
    }

    #[tokio::test]
    async fn invalid_url_format() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "not a url"
        })).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("invalid URL"));
    }

    #[tokio::test]
    async fn reject_non_http() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "ftp://example.com/file"
        })).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("only http/https"));
    }

    #[tokio::test]
    async fn reject_file_url() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "file:///etc/passwd"
        })).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("only http/https"));
    }

    #[tokio::test]
    async fn ssrf_block_private_ip() {
        let tool = WebFetch::new();
        // 10.x.x.x 是私有地址
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "http://10.0.0.1/admin"
        })).await;
        assert!(result.error().is_some());
        let err = result.error().unwrap();
        assert!(err.contains("SSRF") || err.contains("private"), "error: {}", err);
    }

    #[tokio::test]
    async fn ssrf_block_loopback_ipv4() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "http://127.0.0.1:8080/"
        })).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("SSRF"));
    }

    #[tokio::test]
    async fn ssrf_block_loopback_ipv6() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "http://[::1]:8080/"
        })).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("SSRF"));
    }

    #[tokio::test]
    async fn ssrf_block_linklocal() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "http://169.254.169.254/latest/meta-data/"
        })).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("SSRF"));
    }

    #[tokio::test]
    async fn ssrf_block_unspecified() {
        let tool = WebFetch::new();
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "url": "http://0.0.0.0/"
        })).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("SSRF"));
    }

    #[test]
    fn schema_is_valid_json() {
        let tool = WebFetch::new();
        let schema = tool.schema();
        assert!(schema.is_object());
        assert_eq!(schema["type"], "object");
        assert!(tool.read_only());
    }

    #[test]
    fn html_to_text_strips_tags() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b></p></body></html>";
        let text = WebFetch::html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains("<html>"));
        assert!(!text.contains("</b>"));
    }

    #[test]
    fn html_to_text_block_elements() {
        let html = "<p>first</p><p>second</p>";
        let text = WebFetch::html_to_text(html);
        // block elements should have newlines
        assert!(text.contains('\n'));
    }

    #[test]
    fn html_to_text_empty() {
        assert_eq!(WebFetch::html_to_text(""), "");
    }

    #[test]
    fn html_to_text_no_html() {
        let text = "plain text content";
        assert_eq!(WebFetch::html_to_text(text), text);
    }

    #[test]
    fn looks_like_html_detects_doctype() {
        assert!(WebFetch::looks_like_html(b"<!DOCTYPE html>"));
        assert!(WebFetch::looks_like_html(b"<html><body>test</body></html>"));
        assert!(!WebFetch::looks_like_html(b"plain text"));
        assert!(!WebFetch::looks_like_html(b"{\"key\": \"value\"}"));
    }

    #[test]
    fn is_blocked_ip_private() {
        assert!(WebFetch::is_blocked_ip(&"10.0.0.1".parse().unwrap()));
        assert!(WebFetch::is_blocked_ip(&"172.16.0.1".parse().unwrap()));
        assert!(WebFetch::is_blocked_ip(&"192.168.1.1".parse().unwrap()));
        assert!(WebFetch::is_blocked_ip(&"127.0.0.1".parse().unwrap()));
        assert!(WebFetch::is_blocked_ip(&"169.254.1.1".parse().unwrap()));
        assert!(WebFetch::is_blocked_ip(&"::1".parse().unwrap()));
        assert!(WebFetch::is_blocked_ip(&"fc00::".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_public() {
        assert!(!WebFetch::is_blocked_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!WebFetch::is_blocked_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!WebFetch::is_blocked_ip(&"93.184.216.34".parse().unwrap())); // example.com
    }
}
