//! 各 HTTP 推送渠道与 webhook 模板渲染。

use super::*;
use base64::Engine as _;
use pbh_downloader::http::{HttpFetcher, HttpRequest, HttpResponse};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::sync::Arc;

/// 上游 Telegram / Bark / Ntfy 共用的图标地址
/// （`TelegramPushProvider` / `BarkPushProvider` 内联字面量、`NtfyPushProvider.ICON_URL`）。
pub(crate) const ICON_URL: &str = "https://raw.githubusercontent.com/PBH-BTN/PeerBanHelper/refs/heads/master/src/main/resources/assets/icon.png";

/// 对齐各 `loadFromYaml` 内的 `isBlank() → null` 归一化（pushplus 的 topic/channel、
/// serverchan 的 channel/openid）。
pub(crate) fn blank_to_none(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// okhttp `Response.isSuccessful()`：状态码 ∈ [200, 300)。
pub(crate) fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// 当前时间戳（毫秒），对齐 `System.currentTimeMillis()`。
pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 执行 HTTP 请求，网络异常按上游 `IOException` 包装为渠道级错误
/// （各 provider 的 `catch (Exception e) → IllegalStateException("Failed to ...")`）。
pub(crate) async fn execute(
    fetcher: &Arc<dyn HttpFetcher>,
    request: HttpRequest,
    provider: &str,
) -> anyhow::Result<HttpResponse> {
    fetcher
        .execute(request)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send push message to {provider}: {e}"))
}

// ---------------------------------------------------------------------------
// pushplus
// ---------------------------------------------------------------------------

/// 对齐 `PushPlusPushProvider.push`：
/// `POST https://www.pushplus.plus/send`，`Content-Type: application/json`，
/// JSON 体 `{token, [topic], [channel], title, content, template:"markdown"}`（HashMap ⇒ Gson，
/// 空白的 topic/channel 在加载期已置 null，因此**整键不出现**），
/// 成功判据：HTTP 2xx **且** 响应体 `code == 200`。
pub(crate) async fn pushplus_push(
    fetcher: &Arc<dyn HttpFetcher>,
    token: &str,
    topic: Option<&str>,
    channel: Option<&str>,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut args = JsonMap::new();
    args.insert("token".into(), JsonValue::from(token));
    if let Some(topic) = topic {
        args.insert("topic".into(), JsonValue::from(topic));
    }
    if let Some(channel) = channel {
        args.insert("channel".into(), JsonValue::from(channel));
    }
    args.insert("title".into(), JsonValue::from(title));
    args.insert("content".into(), JsonValue::from(content));
    args.insert("template".into(), JsonValue::from("markdown"));

    let request = HttpRequest::post_json(
        "https://www.pushplus.plus/send",
        serde_json::to_string(&JsonValue::Object(args))?,
    );
    let response = execute(fetcher, request, "PushPlus").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to PushPlus: {}",
            response.body
        ));
    }
    let parsed: JsonValue = serde_json::from_str(&response.body).unwrap_or(JsonValue::Null);
    match parsed.get("code").and_then(JsonValue::as_i64) {
        Some(200) => Ok(true),
        Some(_) => Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to PushPlus: {}",
            parsed
                .get("msg")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
        )),
        // 上游此处 `ppr.getCode()` 为 null 会 NPE，并被外层 catch 记为推送失败
        None => Err(anyhow::anyhow!(
            "Failed to send push message to PushPlus: {}",
            response.body
        )),
    }
}

// ---------------------------------------------------------------------------
// serverchan
// ---------------------------------------------------------------------------

/// 对齐 `ServerChanPushProvider.push`：
/// `POST https://sctapi.ftqq.com/<sendkey>.send`，`Content-Type: application/json`，
/// JSON 体 `{title, desp, text, [channel], [openid]}`（channel/openid 为 null 时整键不出现），
/// 成功判据：HTTP 2xx（非 2xx 时按 `ServerChanResponse.message` 报错）。
pub(crate) async fn serverchan_push(
    fetcher: &Arc<dyn HttpFetcher>,
    send_key: &str,
    channel: Option<&str>,
    open_id: Option<&str>,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut map = JsonMap::new();
    map.insert("title".into(), JsonValue::from(title));
    map.insert("desp".into(), JsonValue::from(content));
    map.insert("text".into(), JsonValue::from(title));
    if let Some(channel) = channel {
        map.insert("channel".into(), JsonValue::from(channel));
    }
    if let Some(open_id) = open_id {
        map.insert("openid".into(), JsonValue::from(open_id));
    }

    let request = HttpRequest::post_json(
        format!("https://sctapi.ftqq.com/{send_key}.send"),
        serde_json::to_string(&JsonValue::Object(map))?,
    );
    let response = execute(fetcher, request, "ServerChan").await?;
    if !is_success(response.status) {
        let message = serde_json::from_str::<JsonValue>(&response.body)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(response.body);
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to ServerChan: {message}"
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// telegram
// ---------------------------------------------------------------------------

/// 对齐 `TelegramPushProvider.push`：
/// `POST https://api.telegram.org/bot<token>/sendPhoto`，`Content-Type: application/json`，
/// JSON 体 `{chat_id, caption, text, photo, parse_mode}`，其中正文为 `*<title>*\n<content>`、
/// `photo` 为内置图标地址、`parse_mode` = `Markdown`；
/// 成功判据：HTTP 2xx（非 2xx 时按 `TelegramErrResponse.description` 报错）。
pub(crate) async fn telegram_push(
    fetcher: &Arc<dyn HttpFetcher>,
    token: &str,
    chat_id: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let markdown = format!("*{title}*\n{content}");
    let mut map = JsonMap::new();
    map.insert("chat_id".into(), JsonValue::from(chat_id));
    map.insert("caption".into(), JsonValue::from(markdown.as_str()));
    map.insert("text".into(), JsonValue::from(markdown.as_str()));
    map.insert("photo".into(), JsonValue::from(ICON_URL));
    map.insert("parse_mode".into(), JsonValue::from("Markdown"));

    let request = HttpRequest::post_json(
        format!("https://api.telegram.org/bot{token}/sendPhoto"),
        serde_json::to_string(&JsonValue::Object(map))?,
    );
    let response = execute(fetcher, request, "Telegram").await?;
    if !is_success(response.status) {
        let description = serde_json::from_str::<JsonValue>(&response.body)
            .ok()
            .and_then(|v| {
                v.get("description")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(response.body);
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Telegram: {description}"
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// bark
// ---------------------------------------------------------------------------

/// 对齐 `BarkPushProvider.push`：
/// `POST <backend_url>`（默认 `https://api.day.app/push`），`Content-Type: application/json`，
/// JSON 体 `{title, body, device_key, group, icon}`（`group` 为配置值，空串照发）；
/// 成功判据：HTTP 2xx。
pub(crate) async fn bark_push(
    fetcher: &Arc<dyn HttpFetcher>,
    backend_url: &str,
    device_key: &str,
    message_group: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut map = JsonMap::new();
    map.insert("title".into(), JsonValue::from(title));
    map.insert("body".into(), JsonValue::from(content));
    map.insert("device_key".into(), JsonValue::from(device_key));
    map.insert("group".into(), JsonValue::from(message_group));
    map.insert("icon".into(), JsonValue::from(ICON_URL));

    let request =
        HttpRequest::post_json(backend_url, serde_json::to_string(&JsonValue::Object(map))?);
    let response = execute(fetcher, request, "Bark").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Bark: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// pushdeer
// ---------------------------------------------------------------------------

/// 对齐 `PushDeerPushProvider.push`：
/// `POST <endpoint>`（默认 `https://api2.pushdeer.com/message/push`），
/// `Content-Type: application/json`，
/// JSON 体 `{pushkey, text, desp, type:"markdown"}`；成功判据：HTTP 2xx。
pub(crate) async fn pushdeer_push(
    fetcher: &Arc<dyn HttpFetcher>,
    endpoint: &str,
    push_key: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut map = JsonMap::new();
    map.insert("pushkey".into(), JsonValue::from(push_key));
    map.insert("text".into(), JsonValue::from(title));
    map.insert("desp".into(), JsonValue::from(content));
    map.insert("type".into(), JsonValue::from("markdown"));

    let request = HttpRequest::post_json(endpoint, serde_json::to_string(&JsonValue::Object(map))?);
    let response = execute(fetcher, request, "PushDeer").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to PushDeer: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// gotify
// ---------------------------------------------------------------------------

/// 对齐 `GotifyPushProvider.push`：
/// `POST <endpoint>`（默认 `https://push.example.de/message?token=<apptoken>`），
/// `Content-Type: application/x-www-form-urlencoded`，
/// 表单 `title=<title>&message=<content>&priority=<priority>`；成功判据：HTTP 2xx。
///
/// 注：上游还构造了一个未被使用的 `HashMap`（死代码），此处不移植。
/// Content-Type 由 `HttpRequest` 的表单字段在传输层产生（reqwest 写入
/// `application/x-www-form-urlencoded`），故不再显式追加同名请求头，避免重复头。
pub(crate) async fn gotify_push(
    fetcher: &Arc<dyn HttpFetcher>,
    endpoint: &str,
    priority: i64,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let request = HttpRequest::post_form(
        endpoint,
        vec![
            ("title".to_string(), title.to_string()),
            ("message".to_string(), content.to_string()),
            ("priority".to_string(), priority.to_string()),
        ],
    );
    let response = execute(fetcher, request, "Gotify").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Gotify: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// ntfy
// ---------------------------------------------------------------------------

/// 对齐 `NtfyPushProvider.push`：
/// `POST <server_url>/<topic>`（默认 `https://ntfy.sh`），
/// 体为原始正文（`Content-Type: text/plain; charset=utf-8`，内容即 `content`），
/// 请求头 `Title: =?UTF-8?B?<base64(title)>?=`、`Priority: <配置值>`、`Icon: <图标地址>`，
/// 以及 token 非空白时的 `Authorization: Bearer <token>`、tags 非空白时的 `Tags: <tags>`；
/// 成功判据：HTTP 2xx。
#[allow(clippy::too_many_arguments)] // 逐参对齐上游 NtfyPushProvider 的配置字段
pub(crate) async fn ntfy_push(
    fetcher: &Arc<dyn HttpFetcher>,
    server_url: &str,
    topic: &str,
    token: &str,
    priority: i64,
    tags: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let encoded_title = format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(title.as_bytes())
    );
    let mut request = HttpRequest {
        method: "POST".into(),
        url: format!("{server_url}/{topic}"),
        body: Some(content.to_string()),
        headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
        ..Default::default()
    };
    request = request
        .with_header("Title", &encoded_title)
        .with_header("Priority", &priority.to_string())
        .with_header("Icon", ICON_URL);
    if !token.trim().is_empty() {
        request = request.with_header("Authorization", &format!("Bearer {token}"));
    }
    if !tags.trim().is_empty() {
        request = request.with_header("Tags", tags);
    }

    let response = execute(fetcher, request, "Ntfy").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Ntfy: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// webhook
// ---------------------------------------------------------------------------

/// webhook 一次推送所需的全部输入（字段较多，用结构体传参）。
pub(crate) struct WebhookTask<'a> {
    pub(crate) name: &'a str,
    pub(crate) fetcher: &'a Arc<dyn HttpFetcher>,
    pub(crate) url: &'a str,
    pub(crate) method: &'a str,
    pub(crate) content_type: &'a str,
    pub(crate) body_template: &'a str,
    pub(crate) headers: &'a [(String, String)],
    pub(crate) title: &'a str,
    pub(crate) content: &'a str,
    pub(crate) level: AlertLevel,
    pub(crate) now_ms: i64,
}

/// 对齐 `WebhookPushProvider.push`：URL 与请求体都做模板替换，
/// 成功判据：HTTP 2xx。
pub(crate) async fn webhook_push(task: WebhookTask<'_>) -> anyhow::Result<bool> {
    if task.url.trim().is_empty() {
        return Err(anyhow::anyhow!("Webhook URL cannot be empty"));
    }

    let method = webhook_normalize_method(task.method)?;
    let content_type = webhook_normalize_content_type(task.content_type);
    let rendered_body = render_template(
        task.body_template,
        task.name,
        task.title,
        task.content,
        task.level,
        &content_type,
        false,
        task.now_ms,
    );
    let rendered_url = render_template(
        task.url,
        task.name,
        task.title,
        task.content,
        task.level,
        &content_type,
        true,
        task.now_ms,
    );

    // `createRequestBody`：GET 无请求体；POST 恒有请求体（内容可为空串）
    let body = if method == "GET" {
        None
    } else {
        Some(rendered_body)
    };
    let mut request = HttpRequest {
        method: method.clone(),
        url: rendered_url,
        body,
        ..Default::default()
    };
    // `applyContentType`：非 GET 且有请求体时写入 Content-Type
    if method != "GET" && request.body.is_some() {
        request = request.with_header("Content-Type", &content_type);
    }
    // `applyCustomHeaders`：键非空白、值非 null；值按 RFC 8187 编码非 ASCII
    for (key, value) in task.headers {
        if !key.trim().is_empty() {
            request = request.with_header(key, &encode_header_value(value));
        }
    }

    let response = execute(task.fetcher, request, "Webhook").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP failed while sending push messages to Webhook: {}",
            response.body
        ));
    }
    Ok(true)
}

/// 对齐 `WebhookPushProvider.normalizeMethod`：空 ⇒ POST；仅接受 GET/POST（大写归一）。
pub(crate) fn webhook_normalize_method(method: &str) -> anyhow::Result<String> {
    if method.trim().is_empty() {
        return Ok("POST".to_string());
    }
    let normalized = method.trim().to_uppercase();
    match normalized.as_str() {
        "GET" | "POST" => Ok(normalized),
        _ => Err(anyhow::anyhow!("Unsupported webhook method: {method}")),
    }
}

/// 对齐 `WebhookPushProvider.normalizeContentType`：空/非法 ⇒ `application/json`。
///
/// 上游用 okhttp `MediaType.parse(...).toString()` 做规范化；本移植只做 trim 与
/// `type/subtype` 形状校验（参数原样保留），无法解析时回退默认值。
pub(crate) fn webhook_normalize_content_type(content_type: &str) -> String {
    let trimmed = content_type.trim();
    if trimmed.is_empty() {
        return "application/json".to_string();
    }
    let mut parts = trimmed.splitn(2, '/');
    let (Some(main), Some(sub)) = (parts.next(), parts.next()) else {
        return "application/json".to_string();
    };
    if main.trim().is_empty() || sub.trim().is_empty() {
        return "application/json".to_string();
    }
    trimmed.to_string()
}

/// 对齐 `WebhookPushProvider.renderTemplate`：替换 `{title}`/`{content}`/`{level}`/
/// `{date}`/`{time}`/`{datetime}`/`{channelName}`。
///
/// `url_encode` 为 true（URL 场景）时按 `URLEncoder.encode(..., UTF_8).replace("+", "%20")` 编码；
/// 否则当 content-type 含 `json`（大小写不敏感）时按 JSON 字符串转义后去掉两侧引号。
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_template(
    template: &str,
    channel_name: &str,
    title: &str,
    content: &str,
    level: AlertLevel,
    content_type: &str,
    url_encode: bool,
    now_ms: i64,
) -> String {
    let json = content_type.to_lowercase().contains("json");
    template
        .replace("{title}", &transform_value(title, url_encode, json))
        .replace("{content}", &transform_value(content, url_encode, json))
        .replace("{level}", &transform_value(level.name(), url_encode, json))
        .replace(
            "{date}",
            &transform_value(&format_date_only(now_ms), url_encode, json),
        )
        .replace(
            "{time}",
            &transform_value(&format_time_only(now_ms), url_encode, json),
        )
        .replace(
            "{datetime}",
            &transform_value(&format_date_time(now_ms), url_encode, json),
        )
        .replace(
            "{channelName}",
            &transform_value(channel_name, url_encode, json),
        )
}

/// 对齐 `WebhookPushProvider.transformValue`（`null` 值在 Rust 侧不存在，故不做空判断）。
pub(crate) fn transform_value(value: &str, url_encode: bool, json: bool) -> String {
    if url_encode {
        return url_encode_component(value);
    }
    if !json {
        return value.to_string();
    }
    // 对齐 `JsonUtil.standard().toJson(value)` 后去掉首尾引号（Gson 已 disableHtmlEscaping）
    let quoted = serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""));
    quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or_default()
        .to_string()
}

/// 对齐 `AbstractPushProvider.markdown2Html`（上游用 commonmark-java 的 `HtmlRenderer`；
/// 本移植用 pulldown-cmark 默认选项，同为 CommonMark 规范实现）。
pub(crate) fn markdown_to_html(markdown: &str) -> String {
    let parser = pulldown_cmark::Parser::new(markdown);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

/// 对齐 `TimeUtil.formatDateOnly`：`SimpleDateFormat("yyyy-MM-dd")`（系统时区）。
pub(crate) fn format_date_only(now_ms: i64) -> String {
    format_local(now_ms, "%Y-%m-%d")
}

/// 对齐 `TimeUtil.formatTimeOnly`：`SimpleDateFormat("HH:mm:ss")`（系统时区）。
pub(crate) fn format_time_only(now_ms: i64) -> String {
    format_local(now_ms, "%H:%M:%S")
}

/// 对齐 `TimeUtil.formatDateTime`：`SimpleDateFormat("yyyy-MM-dd HH:mm:ss")`（系统时区）。
pub(crate) fn format_date_time(now_ms: i64) -> String {
    format_local(now_ms, "%Y-%m-%d %H:%M:%S")
}

pub(crate) fn format_local(now_ms: i64, pattern: &str) -> String {
    chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format(pattern)
                .to_string()
        })
        .unwrap_or_default()
}

/// 对齐 `WebhookPushProvider.encodeHeaderValue`（RFC 8187）：
/// 值全为 ASCII（≤ 0x7E）时原样返回；否则前缀 `UTF-8''`，把 > 0x7E 的字节写成大写 `%XX`。
pub(crate) fn encode_header_value(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_string();
    }
    if !value.chars().any(|c| c as u32 > 0x7E) {
        return value.to_string();
    }
    let mut out = String::from("UTF-8''");
    for byte in value.as_bytes() {
        if *byte > 0x7E {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        } else {
            out.push(*byte as char);
        }
    }
    out
}

/// 对齐 `URLEncoder.encode(value, UTF_8).replace("+", "%20")`：
/// 仅 `A-Za-z0-9.-*_` 原样输出，空格写成 `%20`，其余字节写成大写 `%XX`。
pub(crate) fn url_encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
