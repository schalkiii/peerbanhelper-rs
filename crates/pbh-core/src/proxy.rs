//! 出站 HTTP 代理（对齐上游 `config.yml` 的 `proxy:` 段与 `HTTPUtil`）。
//!
//! 实现方式：在进程启动、构造任何 HTTP 客户端之前，把配置写进
//! `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 环境变量——reqwest 的默认
//! 代理识别会读取这些变量，因此下载器、BTN、推送、GeoIP 更新等全部出站请求
//! 自动生效，无需逐个客户端传参。
//!
//! 语义（数值含义以上游**代码**为准：`HTTPUtil.reloadConfig()`，`HTTPUtil.java:96-117`）：
//! - `setting=1`：`host:port` 的 **HTTP** 代理；
//! - `setting=2`：`host:port` 的 **SOCKS5** 代理（写成 `socks5h://`，DNS 也交给代理）；
//! - `setting=0`（默认）：**不额外配置**，保留进程环境。上游此处是 `Proxy.NO_PROXY`
//!   （强制直连）；本移植保留为有意差异——部署环境可能依赖 `HTTP_PROXY`，
//!   清空会切断所有出站请求（2026-10-25 生产部署实测）；
//! - `non-proxy-hosts` 用 `|` 分隔（上游转成正则），本移植转成 `NO_PROXY` 的逗号分隔格式。
//!
//! 已知差异：上游 `pbh.downloader.bypassproxy` 可让下载器请求绕过代理；环境变量方案
//! 无法按客户端区分，故该开关不生效（记录日志提示）。

use crate::config::ProxyConfig;

/// 把代理配置写入进程环境变量（启动期调用一次；修改配置后需重启）。
pub fn apply_env(config: &ProxyConfig) {
    apply_env_with(config, |key, value| match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    });
}

/// 代理配置 → 环境变量变更的纯计算部分。
///
/// `apply(key, Some(v))` 表示设置，`apply(key, None)` 表示清除；由调用方注入，
/// 测试不污染进程环境。
fn apply_env_with(config: &ProxyConfig, mut apply: impl FnMut(&str, Option<&str>)) {
    let host = config
        .host
        .trim()
        .strip_prefix("http://")
        .or_else(|| config.host.trim().strip_prefix("https://"))
        .unwrap_or(config.host.trim());
    match config.setting {
        // 1 = HTTP 代理（上游 `Proxy.Type.HTTP`）
        1 => {
            let url = format!("http://{host}:{}", config.port);
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                apply(key, Some(&url));
            }
            // 环境里遗留的 ALL_PROXY(SOCKS) 会盖过 HTTP_PROXY，必须显式清掉
            for key in ["ALL_PROXY", "all_proxy"] {
                apply(key, None);
            }
            apply_no_proxy(config, &mut apply);
        }
        // 2 = SOCKS5 代理（上游 `Proxy.Type.SOCKS`；`socks5h` 让 DNS 也走代理）
        2 => {
            let url = format!("socks5h://{host}:{}", config.port);
            for key in ["ALL_PROXY", "all_proxy", "SOCKS_PROXY", "socks_proxy"] {
                apply(key, Some(&url));
            }
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                apply(key, None);
            }
            apply_no_proxy(config, &mut apply);
        }
        // 0 / 其它：不额外配置，保留进程现有代理环境（见模块注释里的有意差异）
        _ => {}
    }
    if crate::external_switch::boolean("pbh.downloader.bypassproxy", false) {
        tracing::warn!(
            "pbh.downloader.bypassproxy 在当前实现中不生效（代理通过进程环境变量统一应用）"
        );
    }
}

fn apply_no_proxy(config: &ProxyConfig, apply: &mut impl FnMut(&str, Option<&str>)) {
    let no_proxy = config.non_proxy_hosts.replace('|', ",");
    apply("NO_PROXY", Some(&no_proxy));
    apply("no_proxy", Some(&no_proxy));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(config: &ProxyConfig) -> Vec<(String, Option<String>)> {
        let mut written: Vec<(String, Option<String>)> = Vec::new();
        apply_env_with(config, |key, value| {
            written.push((key.to_string(), value.map(str::to_string)))
        });
        written
    }

    fn get<'a>(written: &'a [(String, Option<String>)], key: &str) -> Option<&'a str> {
        written
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref())
    }

    /// `setting=1` ⇒ HTTP 代理；`setting=2` ⇒ SOCKS5。
    /// 上游以 `HTTPUtil.java:96-117` 的**代码**为准（`config.yml` 的注释与代码矛盾）。
    #[test]
    fn setting_one_is_http_and_two_is_socks5() {
        let http = ProxyConfig {
            setting: 1,
            host: "10.0.0.99".into(),
            port: 9999,
            non_proxy_hosts: "localhost|*.lan".into(),
        };
        let written = capture(&http);
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            assert_eq!(
                get(&written, key),
                Some("http://10.0.0.99:9999"),
                "{key} 应为 HTTP 代理"
            );
        }
        assert_eq!(get(&written, "NO_PROXY"), Some("localhost,*.lan"));
        // 遗留的 ALL_PROXY 必须被清掉，否则会盖过 HTTP_PROXY
        assert_eq!(
            written
                .iter()
                .find(|(k, _)| k == "ALL_PROXY")
                .map(|(_, v)| v),
            Some(&None)
        );

        let socks = ProxyConfig {
            setting: 2,
            host: "socks.local".into(),
            port: 1080,
            non_proxy_hosts: String::new(),
        };
        let written = capture(&socks);
        assert_eq!(
            get(&written, "ALL_PROXY"),
            Some("socks5h://socks.local:1080"),
            "setting=2 是 SOCKS5 而不是 HTTP"
        );
        assert_eq!(
            written
                .iter()
                .find(|(k, _)| k == "HTTP_PROXY")
                .map(|(_, v)| v),
            Some(&None),
            "HTTP 代理变量应被清除"
        );

        // 0 = 不额外配置：不能清空部署环境已有的代理（不产生任何写入）
        let reset = ProxyConfig {
            setting: 0,
            ..ProxyConfig::default()
        };
        assert!(capture(&reset).is_empty(), "setting=0 不应改写环境变量");
    }
}
