//! 出站 HTTP 代理（对齐上游 `config.yml` 的 `proxy:` 段与 `HTTPUtil`）。
//!
//! 实现方式：在进程启动、构造任何 HTTP 客户端之前，把配置写进
//! `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` 环境变量——reqwest 的默认代理
//! 识别会读取这些变量，因此下载器、BTN、推送、GeoIP 更新等全部出站请求
//! 自动生效，无需逐个客户端传参。
//!
//! 语义：
//! - `setting=0`：不额外配置代理，**保留进程环境**（部署环境可能依赖 `HTTP_PROXY`；
//!   清空会切断所有出站请求——2026-10-25 生产部署实测）；
//! - `setting=1`：保持系统/环境代理（reqwest 默认行为）；
//! - `setting=2`：使用 `host:port` HTTP 代理，`non-proxy-hosts` 用 `|` 分隔，
//!   转换为 `NO_PROXY` 的逗号分隔格式。
//!
//! 已知差异：上游 `setting=0` 表示强制不使用代理；本移植为兼容部署环境将其定义为
//! 「不额外配置」。另：上游 `pbh.downloader.bypassproxy` 可让下载器请求绕过代理；
//! 环境变量方案无法按客户端区分，故该开关不生效（记录日志提示）。

use crate::config::ProxyConfig;

/// 把代理配置写入进程环境变量（启动期调用一次；修改配置后需重启）。
pub fn apply_env(config: &ProxyConfig) {
    match config.setting {
        0 | 1 => {
            // 0 = 不额外配置；1 = 跟随环境。两者都保留进程现有代理环境——
            // 清空会让依赖 `HTTP_PROXY` 的部署直接失去网络（实测订阅源超时）。
        }
        2 => {
            let host = config.host.trim();
            let url = if host.starts_with("http://") || host.starts_with("https://") {
                format!("{host}:{}", config.port)
            } else {
                format!("http://{host}:{}", config.port)
            };
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                std::env::set_var(key, &url);
            }
            let no_proxy = config.non_proxy_hosts.replace('|', ",");
            std::env::set_var("NO_PROXY", &no_proxy);
            std::env::set_var("no_proxy", &no_proxy);
        }
        _ => {}
    }
    if crate::external_switch::boolean("pbh.downloader.bypassproxy", false) {
        tracing::warn!(
            "pbh.downloader.bypassproxy 在当前实现中不生效（代理通过进程环境变量统一应用）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仅在当前测试进程内修改环境变量；使用唯一配置避免并行测试互相影响。
    #[test]
    fn setting_two_sets_proxy_and_zero_preserves_environment() {
        let config = ProxyConfig {
            setting: 2,
            host: "10.0.0.99".into(),
            port: 9999,
            non_proxy_hosts: "localhost|*.lan".into(),
        };
        apply_env(&config);
        assert_eq!(
            std::env::var("HTTP_PROXY").unwrap(),
            "http://10.0.0.99:9999"
        );
        assert_eq!(std::env::var("NO_PROXY").unwrap(), "localhost,*.lan");
        // 0 = 不额外配置：不能清空部署环境已有的代理
        let reset = ProxyConfig {
            setting: 0,
            ..ProxyConfig::default()
        };
        apply_env(&reset);
        assert_eq!(
            std::env::var("HTTP_PROXY").unwrap(),
            "http://10.0.0.99:9999"
        );
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            std::env::remove_var(key);
        }
    }
}
