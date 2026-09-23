//! 最小异步 HTTP 抽象：生产用 reqwest，测试用按 URL 返回夹具的内存实现。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

/// 显式 boxed future，使 trait 同时满足 `dyn` 兼容与 `Send`。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Default)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    /// 自定义请求头（如 `X-Transmission-Session-Id`）
    pub headers: Vec<(String, String)>,
    /// application/x-www-form-urlencoded 表单
    pub form: Option<Vec<(String, String)>>,
    /// 原始请求体（JSON-RPC 等）
    pub body: Option<String>,
    pub bearer: Option<String>,
    pub basic: Option<(String, String)>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            ..Default::default()
        }
    }
    pub fn post_form(url: impl Into<String>, form: Vec<(String, String)>) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            form: Some(form),
            ..Default::default()
        }
    }
    pub fn post_json(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            body: Some(body.into()),
            headers: vec![("Content-Type".into(), "application/json".into())],
            ..Default::default()
        }
    }
    /// 追加请求头（同名则覆盖）。
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 极简异步 HTTP 客户端接口（cookie 会话由实现负责）。
pub trait HttpFetcher: Send + Sync {
    fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>>;
}

/// reqwest 生产实现，启用 cookie store 以维持 qB 会话。
pub struct ReqwestFetcher {
    client: reqwest::Client,
    pub last_cookies: Mutex<HashMap<String, String>>,
}

impl ReqwestFetcher {
    pub fn new(
        verify_tls: bool,
        connect_timeout_secs: u64,
        timeout_secs: u64,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .danger_accept_invalid_certs(!verify_tls)
            .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .user_agent("PeerBanHelper-RS")
            .build()?;
        Ok(Self {
            client,
            last_cookies: Mutex::new(HashMap::new()),
        })
    }
}

impl HttpFetcher for ReqwestFetcher {
    fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
        Box::pin(async move {
            // 非法 method 直接报错，而不是静默降级成 GET（否则 PUT 被写错时请求语义会变）
            let method: reqwest::Method = req
                .method
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid http method: {}", req.method))?;
            let mut builder = self.client.request(method, &req.url);
            if let Some((u, p)) = req.basic {
                builder = builder.basic_auth(u, Some(p));
            }
            if let Some(token) = req.bearer {
                builder = builder.bearer_auth(token);
            }
            for (name, value) in &req.headers {
                builder = builder.header(name, value);
            }
            if let Some(form) = req.form {
                builder = builder.form(&form);
            }
            if let Some(body) = req.body {
                builder = builder.body(body);
            }
            let resp = builder.send().await?;
            let status = resp.status().as_u16();
            let headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
                .collect();
            // 读取响应体失败要向上抛（对齐 OkHttp 的 `IOException`），
            // 静默换成空串会把「读取失败」伪装成「空响应」
            let body = resp.text().await?;
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}
