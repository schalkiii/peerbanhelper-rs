//! 黄金测试公共设施：按 URL 返回夹具的内存 HttpFetcher，并记录封禁请求。

#![allow(dead_code)]

use pbh_downloader::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse};
use pbh_downloader::qbittorrent::{QBConfig, QBittorrentDownloader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RecordedCall {
    pub url: String,
    pub form: Option<Vec<(String, String)>>,
    /// 该请求是否携带 HTTP Basic 认证
    pub basic: bool,
}

pub struct MockFetcher {
    fixtures: PathBuf,
    pub calls: Mutex<Vec<RecordedCall>>,
    /// 动态 torrentPeers 响应（多波测试用），按调用次序返回
    pub peers_responses: Mutex<Vec<String>>,
    /// 模拟 qB 的会话状态：未登录时 /app/buildInfo 返回 403
    pub logged_in: AtomicBool,
    /// 模拟 `/sync/maindata` 尚未就绪（alltime_ul/dl 均为 0）
    pub maindata_ready: AtomicBool,
    /// `/app/version` 返回值（用于测试按版本判定的下载器能力标志）
    pub version: Mutex<String>,
    /// 模拟反代要求 Basic 认证：未携带 Basic 的请求一律返回 401
    pub basic_required: AtomicBool,
}

impl MockFetcher {
    pub fn new(fixtures: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            fixtures,
            calls: Mutex::new(Vec::new()),
            peers_responses: Mutex::new(Vec::new()),
            logged_in: AtomicBool::new(false),
            maindata_ready: AtomicBool::new(true),
            version: Mutex::new("v5.0.2".to_string()),
            basic_required: AtomicBool::new(false),
        })
    }

    /// 携带 Basic 认证的请求次数
    pub fn call_count_with_basic(&self, needle: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.url.contains(needle) && c.basic)
            .count()
    }

    /// 覆盖 `/app/version` 返回值。
    pub fn set_version(&self, v: &str) {
        *self.version.lock().unwrap() = v.to_string();
    }

    /// 统计对某路径的请求次数
    pub fn call_count(&self, needle: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| c.url.contains(needle)).count()
    }

    fn fixture(&self, name: &str) -> String {
        std::fs::read_to_string(self.fixtures.join(name)).unwrap_or_default()
    }

    pub fn recorded(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    /// 返回所有 /transfer/banPeers 的 peers 载荷
    pub fn ban_payloads(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.url.contains("/transfer/banPeers"))
            .filter_map(|c| {
                c.form
                    .as_ref()
                    .and_then(|f| f.iter().find(|(k, _)| k == "peers").map(|(_, v)| v.clone()))
            })
            .collect()
    }

    /// 返回所有 setPreferences 的 json 载荷
    pub fn set_prefs_payloads(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.url.contains("/app/setPreferences"))
            .filter_map(|c| {
                c.form
                    .as_ref()
                    .and_then(|f| f.iter().find(|(k, _)| k == "json").map(|(_, v)| v.clone()))
            })
            .collect()
    }
}

impl HttpFetcher for MockFetcher {
    fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(RecordedCall {
                url: req.url.clone(),
                form: req.form.clone(),
                basic: req.basic.is_some(),
            });
            let url = req.url.as_str();
            // 反代需要 Basic 认证：未携带则返回 401（对齐 OkHttp Authenticator 的触发条件）
            if self.basic_required.load(Ordering::SeqCst) && req.basic.is_none() {
                return Ok(HttpResponse::new(401, String::new()));
            }

            let body = if url.contains("/auth/login") {
                self.logged_in.store(true, Ordering::SeqCst);
                "Ok.".to_string()
            } else if url.contains("/app/buildInfo") {
                // 与 qB 一致：未认证时该端点返回 403
                if !self.logged_in.load(Ordering::SeqCst) {
                    return Ok(HttpResponse::new(403, String::new()));
                }
                self.fixture("buildInfo.json")
            } else if url.contains("/app/version") {
                self.version.lock().unwrap().clone()
            } else if url.contains("/app/setPreferences") || url.contains("/transfer/banPeers") {
                String::new()
            } else if url.contains("/sync/torrentPeers") {
                let mut queue = self.peers_responses.lock().unwrap();
                if !queue.is_empty() {
                    queue.remove(0)
                } else {
                    self.fixture("torrentPeers.json")
                }
            } else if url.contains("/torrents/properties") {
                self.fixture("properties.json")
            } else if url.contains("/torrents/info") {
                self.fixture("torrents_info.json")
            } else if url.contains("/sync/maindata") {
                if self.maindata_ready.load(Ordering::SeqCst) {
                    self.fixture("maindata.json")
                } else {
                    r#"{"server_state":{"alltime_ul":0,"alltime_dl":0}}"#.to_string()
                }
            } else {
                return Ok(HttpResponse::new(404, String::new()));
            };
            Ok(HttpResponse::new(200, body))
        })
    }
}

/// 用内存 Mock 构建一个 qB 下载器。
pub fn mock_qb(fetcher: Arc<MockFetcher>) -> Arc<QBittorrentDownloader> {
    mock_qb_with(fetcher, QBConfig::default())
}

/// 用指定的 QBConfig（除 endpoint/id 外）构建 qB 下载器。
pub fn mock_qb_with(fetcher: Arc<MockFetcher>, mut base: QBConfig) -> Arc<QBittorrentDownloader> {
    base.id = "qb-test".into();
    base.name = "qB Test".into();
    base.endpoint = "http://mock.local".into();
    if base.username.is_empty() {
        base.username = "admin".into();
        base.password = "admin".into();
    }
    Arc::new(
        QBittorrentDownloader::with_fetcher(base, fetcher.clone() as Arc<dyn HttpFetcher>, Duration::from_secs(3600))
            .unwrap(),
    )
}

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}
