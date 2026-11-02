//! AutoSTUN 应用层编排（对齐上游 `BTStunManager` + `BTStunInstance`）。
//!
//! 上游由 Spring 组件 `BTStunManager` 每 5 秒巡检一次：对 `auto-stun.downloaders`
//! 列表里的下载器，只在 `lastStatus == HEALTHY`、登录成功、且具备
//! `LIVE_UPDATE_BT_PROTOCOL_PORT` 能力时才注册；注册即建立 STUN 隧道并把下载器的 BT
//! 监听端口改写成映射到公网的端口。本模块按其语义逐条移植：
//!
//! - `auto-stun.enabled=false` ⇒ **不创建任何线程 / socket**（严格 no-op，
//!   与上游「不注册任何 provider」一致）；
//! - 每 5 秒：注销不在配置里的实例、跳过未在线的下载器、
//!   `!HEALTHY && 失败次数 > 10` 时注销（对齐 `failedLoginAttempts > 10`）；
//!   已注册且隧道有效的实例直接跳过（`tunnel.isValid()`）；
//! - 隧道失效/首次注册：关旧隧道 → 登录 → `create_tunnel`（含 `stun.available-test`
//!   与 LAN 主机校验）→ **仅当端口不同才** `set_bt_protocol_port(outer.port)`；
//!   端口改写失败 ⇒ 注销（上游异常即 `unregister`）；
//! - 建立的隧道登记进 [`AutoStun`] 注册表，`translate` 先查隧道连接表再查静态映射；
//! - 退出/停用时关闭全部隧道并排出注册表。
//!
//! 已知差异：上游 `use-friendly-loopback-mapping` 在 v9.5.3 后端**无人消费**
//! （真实开关是 ExternalSwitch `pbh.TCPForwarder.useFriendlyAddressForLoopback`），
//! 本移植同样只做配置读写。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pbh_core::auto_stun::{AutoStun, AutoStunSettings};
use pbh_core::auto_stun_forwarder::TunnelConfig;
use pbh_downloader::Downloader;
use pbh_web::DownloaderStatus;
use tracing::{info, warn};

use crate::wave::{DownloaderEntry, LoginGate};

/// 巡检间隔（上游 `scheduleWithFixedDelay(scanAndLoad, 0, 5, SECONDS)`）。
const SCAN_INTERVAL: Duration = Duration::from_secs(5);
/// 停用等待的轮询粒度（保证 `close()` 不被 5s 睡眠拖住）。
const STOP_POLL: Duration = Duration::from_millis(200);
/// 未 HEALTHY 且连续登录失败超过该值即注销（上游 `failedLoginAttempts > 10`）。
const UNREGISTER_AFTER_FAILURES: u32 = 10;
/// 上游 `DownloaderFeature.LIVE_UPDATE_BT_PROTOCOL_PORT` 的线上值。
const CAPABILITY_LIVE_UPDATE_BT_PORT: &str = "LIVE_UPDATE_BT_PROTOCOL_PORT";

struct Instance {
    tunnel: Arc<pbh_core::auto_stun_forwarder::StunTcpTunnel>,
}

/// AutoSTUN 编排句柄（不启用时不持有线程）。
pub struct AutoStunManager {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AutoStunManager {
    /// 启动编排线程。`registry` 必须已挂在下载器的 `IpRemapConfig` 上（建下载器时 clone），
    /// 否则隧道连接表无法参与 peer 地址翻译。
    pub fn spawn(
        registry: Option<Arc<AutoStun>>,
        settings: Arc<dyn Fn() -> AutoStunSettings + Send + Sync>,
        entries: Arc<StdMutex<Vec<DownloaderEntry>>>,
        statuses: Arc<StdMutex<Vec<DownloaderStatus>>>,
        login_gates: Arc<StdMutex<HashMap<String, LoginGate>>>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        // 没有注册表 ⇒ 连空转线程都不需要（配置里根本没有 auto-stun 支持）。
        let Some(registry) = registry else {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("auto-stun".to_string())
            .spawn(move || {
                run_loop(
                    &flag,
                    registry,
                    settings,
                    entries,
                    statuses,
                    login_gates,
                    runtime,
                )
            })
            .ok();
        Self { stop, handle }
    }

    /// 停止巡检并等待线程退出（线程退出前会关闭全部隧道）。
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AutoStunManager {
    fn drop(&mut self) {
        self.close();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    stop: &AtomicBool,
    registry: Arc<AutoStun>,
    settings: Arc<dyn Fn() -> AutoStunSettings + Send + Sync>,
    entries: Arc<StdMutex<Vec<DownloaderEntry>>>,
    statuses: Arc<StdMutex<Vec<DownloaderStatus>>>,
    login_gates: Arc<StdMutex<HashMap<String, LoginGate>>>,
    runtime: tokio::runtime::Handle,
) {
    let mut instances: HashMap<String, Instance> = HashMap::new();
    let mut warned_empty_servers = false;
    while !stop.load(Ordering::SeqCst) {
        let settings = settings();
        // 开关跟随配置热更新：关闭时立刻关闭全部隧道并保持直通
        registry.set_enabled(settings.enabled);
        if !settings.enabled {
            for (_, instance) in instances.drain() {
                instance.tunnel.close();
            }
            registry.clear_tunnels();
            std::thread::sleep(SCAN_INTERVAL.min(STOP_POLL));
            continue;
        }
        if settings.stun_servers.is_empty() && !warned_empty_servers {
            warn!("AutoSTUN 已启用但 stun.tcp-servers 为空：不会建立任何隧道（翻译保持直通）");
            warned_empty_servers = true;
        }
        let configured: Vec<String> = settings.downloaders.clone();
        let downloaders: Vec<Arc<dyn Downloader>> = entries
            .lock()
            .map(|entries| entries.iter().map(|e| e.downloader.clone()).collect())
            .unwrap_or_default();

        // 1) 不再配置的下载器：注销并关闭隧道
        instances.retain(|id, instance| {
            if configured.iter().any(|item| item == id) {
                return true;
            }
            instance.tunnel.close();
            info!("AutoSTUN 已移除下载器 {id} 的隧道（不再在 auto-stun.downloaders 中）");
            false
        });

        for downloader in downloaders {
            let id = downloader.id().to_string();
            if !configured.iter().any(|item| item == &id) {
                continue;
            }
            let online = statuses
                .lock()
                .map(|list| {
                    list.iter()
                        .find(|status| status.id == id)
                        .map(|status| status.online)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let failures = login_gates
                .lock()
                .ok()
                .and_then(|gates| gates.get(&id).map(LoginGate::attempts))
                .unwrap_or(0);
            if !online {
                if failures > UNREGISTER_AFTER_FAILURES {
                    if let Some(instance) = instances.remove(&id) {
                        instance.tunnel.close();
                        warn!("AutoSTUN 已注销下载器 {id}（连续登录失败 {failures} 次）");
                    }
                }
                continue;
            }
            // 已注册且隧道有效 ⇒ 本轮无事可做
            if let Some(instance) = instances.get(&id) {
                if instance.tunnel.is_valid() {
                    continue;
                }
                // 失效：关掉后重建
                if let Some(old) = instances.remove(&id) {
                    old.tunnel.close();
                    info!("AutoSTUN 隧道失效，正在为下载器 {id} 重建");
                }
            }
            // 注册门槛：必须声明 LIVE_UPDATE_BT_PROTOCOL_PORT（Deluge 等不支持）
            if !downloader
                .feature_flags()
                .iter()
                .any(|flag| flag == CAPABILITY_LIVE_UPDATE_BT_PORT)
            {
                continue;
            }
            // 注册前先登录（上游 register 内 `downloader.login()` 失败即放弃）
            let logged_in = runtime
                .block_on(downloader.login())
                .map(|result| result.success)
                .unwrap_or(false);
            if !logged_in {
                continue;
            }
            let host = host_from_endpoint(downloader.endpoint());
            let mut tunnel_config = TunnelConfig {
                available_test: settings.available_test,
                // 上游 `pbh.btstun.localPort.<downloaderId>`（默认 0 ⇒ 随机可用端口）
                local_port: pbh_core::external_switch::integer(
                    &format!("pbh.btstun.localPort.{id}"),
                    0,
                )
                .clamp(0, u16::MAX as i64) as u16,
                ..TunnelConfig::default()
            };
            let _ = &mut tunnel_config;
            let Some(tunnel) = registry.create_tunnel(&settings.stun_servers, &host, &tunnel_config) else {
                continue;
            };
            let target_port = tunnel.outer.port();
            let current = runtime
                .block_on(downloader.bt_protocol_port())
                .unwrap_or(None);
            if current != Some(target_port) {
                if let Err(e) = runtime.block_on(downloader.set_bt_protocol_port(target_port)) {
                    // 上游：改写端口异常 ⇒ unregister
                    warn!("AutoSTUN 改写下载器 {id} 的 BT 端口失败，放弃隧道: {e}");
                    tunnel.close();
                    continue;
                }
                info!(
                    "AutoSTUN 已把下载器 {id} 的 BT 端口改写为映射端口 {target_port}（原 {:?}）",
                    current
                );
            }
            info!(
                "AutoSTUN 已为下载器 {} 建立隧道: 本机 {} → 公网 {}",
                downloader.name(),
                tunnel.inter,
                tunnel.outer
            );
            let tunnel = Arc::new(tunnel);
            registry.register_tunnel(id.clone(), tunnel.clone());
            instances.insert(id, Instance { tunnel });
        }

        // 可中断的 5 秒等待
        let mut waited = Duration::ZERO;
        while waited < SCAN_INTERVAL && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(STOP_POLL);
            waited += STOP_POLL;
        }
    }

    // 退出：关闭全部隧道并排出注册表
    for (_, instance) in instances.drain() {
        instance.tunnel.close();
    }
    registry.clear_tunnels();
    info!("AutoSTUN 巡检线程已退出，隧道已全部关闭");
}

/// 从下载器 endpoint 取主机名（`scheme://host[:port]` ⇒ `host`，IPv6 去方括号）。
fn host_from_endpoint(endpoint: &str) -> String {
    let rest = endpoint
        .split("://")
        .nth(1)
        .unwrap_or(endpoint)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = rest.rsplit_once(':').map(|(host, _)| host).unwrap_or(rest);
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_host_is_extracted_without_port_or_brackets() {
        assert_eq!(host_from_endpoint("http://127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_from_endpoint("https://qb.local"), "qb.local");
        assert_eq!(host_from_endpoint("http://[::1]:8080/api"), "::1");
        assert_eq!(host_from_endpoint("192.168.1.2:9091"), "192.168.1.2");
    }
}
