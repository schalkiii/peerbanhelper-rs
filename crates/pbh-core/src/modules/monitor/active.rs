// ===========================================================================
// active-monitoring
// ===========================================================================

/// `module.active-monitoring` 的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveMonitoringSettings {
    /// `traffic-monitoring.daily`（对齐 `getLong("traffic-monitoring.daily", -1)`）
    pub daily_traffic_capping: i64,
    /// `traffic-sliding-capping.enabled`（对齐 `getBoolean(...)`，缺省 false）
    pub use_traffic_sliding_capping: bool,
    /// `traffic-sliding-capping.daily-max-allowed-upload-traffic`（对齐 `getLong(...)`，缺省 0）
    pub max_traffic_allowed_in_window_period: i64,
    /// `traffic-sliding-capping.max-speed`
    pub traffic_sliding_capping_max_speed: i64,
    /// `traffic-sliding-capping.min-speed`
    pub traffic_sliding_capping_min_speed: i64,
}

impl Default for ActiveMonitoringSettings {
    fn default() -> Self {
        Self {
            daily_traffic_capping: -1,
            use_traffic_sliding_capping: false,
            max_traffic_allowed_in_window_period: 0,
            traffic_sliding_capping_max_speed: 0,
            traffic_sliding_capping_min_speed: 0,
        }
    }
}

/// 对齐 `DownloaderSpeedLimiter`（上传/下载速率上限，bytes/s）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpeedLimiter {
    pub upload: i64,
    pub download: i64,
}

/// 一个下载器在 `updateTrafficStatus` 时刻的状态快照（对齐上游依次调用的
/// `downloader.login()` / `getStatistics()` / `getSpeedLimiter()` / `getFeatureFlags()`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloaderTrafficStats {
    pub id: String,
    /// 展示名（仅用于日志）
    pub name: String,
    /// `downloader.login().success()`：未成功登录的下载器本轮跳过（上游在 catch 中记日志）
    pub logged_in: bool,
    pub total_downloaded: i64,
    pub total_uploaded: i64,
    /// `None` 对齐 `getSpeedLimiter() == null`（不支持限速）-> 跳过
    pub speed_limiter: Option<SpeedLimiter>,
    /// `featureFlags.contains(TRAFFIC_STATS)`：不上报流量的下载器不参与滑动窗口限速
    pub traffic_stats_feature: bool,
}

impl DownloaderTrafficStats {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        logged_in: bool,
        total_downloaded: i64,
        total_uploaded: i64,
        speed_limiter: Option<SpeedLimiter>,
        traffic_stats_feature: bool,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            logged_in,
            total_downloaded,
            total_uploaded,
            speed_limiter,
            traffic_stats_feature,
        }
    }
}

/// 对齐上游 `TrafficJournalServiceImpl.SlidingWindowDynamicSpeedLimiter`
/// （字段一一对应；Java 的 `Long`/`Double` 装箱类型 -> `Option`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SlidingWindowDynamicSpeedLimiter {
    pub window_size_millis: i64,
    pub window_start_time: i64,
    pub uploaded_in_window: i64,
    pub old_speed_limit: i64,
    pub new_speed_limit: i64,
    pub threshold: i64,
    pub min_speed: i64,
    pub max_speed: i64,
    pub increase_factor: Option<f64>,
    pub decrease_factor: Option<f64>,
    /// 上游只把 `newSpeed` 设为 maxSpeed，**没有**给该字段赋值，因此恒为 `None`
    /// （保留字段以对齐结构）。
    pub reached_maximum_speed: Option<bool>,
    pub reached_minimum_speed: Option<bool>,
}

/// 一次滑动窗口限速调整的落地结果（对齐
/// `new DownloaderSpeedLimiter(calculatedData.getNewSpeedLimit(), speedLimiter.download())`
/// 后的 `downloader.setSpeedLimiter(...)`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SpeedLimitChange {
    pub downloader_id: String,
    pub downloader_name: String,
    /// 需要下发的新的速率限制（上传取新值，下载保持原值）
    pub limiter: SpeedLimiter,
    pub computed: SlidingWindowDynamicSpeedLimiter,
}

/// 对齐 `TrafficJournalService#tweakSpeedLimiterBySlidingWindow` 的**纯计算**部分。
///
/// 参数：窗口内已上传流量、当前上传限速、阈值、最小/最大限速、当前时间。
/// 窗口固定 24 小时（上游 `Duration.ofHours(24)`），窗口起点 = `now_ms - 24h`。
pub fn compute_sliding_window_limiter(
    uploaded_in_window: i64,
    old_speed_limit: i64,
    threshold: i64,
    min_speed: i64,
    max_speed: i64,
    now_ms: i64,
) -> SlidingWindowDynamicSpeedLimiter {
    let window_size_millis = SLIDING_WINDOW_MILLIS;
    let mut result = SlidingWindowDynamicSpeedLimiter {
        window_size_millis,
        window_start_time: now_ms - window_size_millis,
        uploaded_in_window,
        old_speed_limit,
        new_speed_limit: 0,
        threshold,
        min_speed,
        max_speed,
        increase_factor: None,
        decrease_factor: None,
        reached_maximum_speed: None,
        reached_minimum_speed: None,
    };

    let new_speed;
    if uploaded_in_window >= threshold {
        // 应用节流策略 - 当达到或超过阈值时
        if old_speed_limit <= min_speed {
            new_speed = min_speed;
            result.reached_minimum_speed = Some(true);
        } else {
            // 计算减少因子 b = l_current / l
            let b = if threshold > 0 {
                uploaded_in_window as f64 / threshold as f64
            } else {
                1.0
            };
            let reduced = (old_speed_limit as f64 / b).trunc() as i64;
            new_speed = min_speed.max(reduced);
            result.decrease_factor = Some(b);
        }
    } else if old_speed_limit >= max_speed && max_speed > 0 {
        // 应用解除节流策略 - 未达到阈值，且当前限速已达上限
        new_speed = max_speed;
    } else {
        // 计算增加因子 a = (l - l_current) / w，并转换为每秒字节数
        let a = (threshold - uploaded_in_window) as f64 / window_size_millis as f64 * 1000.0;
        // 上游这里是 Math.addExact(...)（溢出抛异常）；本移植饱和处理以避免 panic
        let increased = old_speed_limit.saturating_add(a.trunc() as i64);
        new_speed = max_speed.min(increased);
        result.increase_factor = Some(a);
    }
    result.new_speed_limit = new_speed;
    result
}

/// 对齐上游 `ActiveMonitoringModule`（配置键 `active-monitoring`）。
///
/// - 每小时把各下载器流量快照写入 `traffic_journal_v3`（[`Self::on_tick`]）；
/// - `traffic-monitoring.daily > 0` 时，当日上传总量达到阈值即发一次 WARN 告警
///   （identifier = `dataTrafficCapping-<当日 0 点的 epoch 秒>`，见
///   [`Self::update_traffic_monitoring`]）；
/// - `traffic-sliding-capping.enabled` 时按 24 小时滑动窗口调整下载器上传限速
///   （[`Self::update_traffic_capping_service`]）；
/// - `check` 恒 `pass()`，绝不封禁。
pub struct ActiveMonitoringModule {
    pub settings: ActiveMonitoringSettings,
    sink: Arc<dyn MonitorSink>,
}

impl ActiveMonitoringModule {
    pub fn new(sink: Arc<dyn MonitorSink>, settings: ActiveMonitoringSettings) -> Self {
        Self { settings, sink }
    }

    /// 对齐 `updateTrafficStatus()`：写流量日志 → 跑阈值告警 → 跑滑动窗口限速。
    ///
    /// 上游由 `registerScheduledTask(this::updateTrafficStatus, 0, 1, MINUTES)` 每 1 分钟调用。
    /// 本移植无需 `&mut self`：全部状态（含告警去重）都在 [`MonitorSink`] 内。
    ///
    /// 返回 `(限速变更, 本次新发布的日流量阈值告警)`：告警的落库已在
    /// [`MonitorSink::publish_alert`] 完成；`push = true` 的推送分发由应用层接线
    /// （对齐上游 `AlertManagerImpl.publishAlert(push=true)` 的推送分支，见 pbh 的 `MonitorHost`）。
    pub fn on_tick(
        &self,
        downloaders: &[DownloaderTrafficStats],
        now_ms: i64,
    ) -> (Vec<SpeedLimitChange>, Option<TrafficMonitoringAlert>) {
        for stats in downloaders {
            // 上游：`if (downloader.login().success())`，失败在 catch 中记日志
            if !stats.logged_in {
                continue;
            }
            self.sink.update_traffic_journal(
                &stats.id,
                stats.total_downloaded,
                stats.total_uploaded,
                0,
                0,
                now_ms,
            );
        }
        let alert = self.update_traffic_monitoring(now_ms);
        let changes = self.update_traffic_capping_service(downloaders, now_ms);
        (changes, alert)
    }
    /// 对齐 `updateTrafficMonitoringService()`：当日上传流量超阈值 → 发布一次告警。
    pub fn update_traffic_monitoring(&self, now_ms: i64) -> Option<TrafficMonitoringAlert> {
        if self.settings.daily_traffic_capping <= 0 {
            return None;
        }
        let start_of_today = start_of_today_ms(now_ms);
        let data = self
            .sink
            .query_traffic_overall(None, start_of_today, end_of_today_ms(now_ms));
        let total_bytes: i64 = data.iter().map(|d| d.data_overall_uploaded).sum();
        // 上游 identifier 用的是 `getStartOfToday(now).toEpochSecond()`
        let identifier = format!("dataTrafficCapping-{}", start_of_today / 1000);
        if total_bytes < self.settings.daily_traffic_capping {
            return None;
        }
        // 一天只发一次
        if self.sink.alert_exists_include_read(&identifier) {
            return None;
        }
        let title = TranslationComponent::with_params(
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_TITLE",
            vec![format_date_only(now_ms).into()],
        );
        let description = TranslationComponent::with_params(
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_DESCRIPTION",
            vec![
                format_date_time(now_ms).into(),
                human_readable_byte_count_bin(total_bytes).into(),
                human_readable_byte_count_bin(self.settings.daily_traffic_capping).into(),
            ],
        );
        let alert = AlertRecord {
            push: true,
            level: AlertLevel::Warn,
            identifier,
            title,
            description,
        };
        self.sink.publish_alert(
            alert.push,
            alert.level,
            &alert.identifier,
            &alert.title,
            &alert.description,
        );
        Some(TrafficMonitoringAlert {
            alert,
            total_uploaded: total_bytes,
            threshold: self.settings.daily_traffic_capping,
        })
    }

    /// 对齐 `updateTrafficCappingService()`：逐个下载器计算并下发新的上传限速。
    ///
    /// 跳过条件逐条保留：未登录成功、`getSpeedLimiter() == null`、
    /// 特性标志中没有 `TRAFFIC_STATS`。
    pub fn update_traffic_capping_service(
        &self,
        downloaders: &[DownloaderTrafficStats],
        now_ms: i64,
    ) -> Vec<SpeedLimitChange> {
        if !self.settings.use_traffic_sliding_capping {
            return Vec::new();
        }
        let mut changes = Vec::new();
        for stats in downloaders {
            if !stats.logged_in {
                continue;
            }
            let Some(limiter) = stats.speed_limiter else {
                continue;
            };
            if !stats.traffic_stats_feature {
                continue;
            }
            // 上游这里传的是 `null`（对所有下载器合并统计）
            let computed = self.tweak_speed_limiter_by_sliding_window(None, limiter, now_ms);
            let new_limiter = SpeedLimiter {
                upload: computed.new_speed_limit,
                download: limiter.download,
            };
            tracing::debug!(
                "active-monitoring: 下载器 {} 上传限速 {} -> {}（{}）",
                stats.name,
                human_readable_byte_count_bin(limiter.upload),
                human_readable_byte_count_bin(new_limiter.upload),
                human_readable_byte_count_si(new_limiter.upload),
            );
            changes.push(SpeedLimitChange {
                downloader_id: stats.id.clone(),
                downloader_name: stats.name.clone(),
                limiter: new_limiter,
                computed,
            });
        }
        changes
    }

    /// 对齐 `trafficJournalDao.tweakSpeedLimiterBySlidingWindow(downloader, speedLimiter, ...)`：
    /// 先按窗口查询流量，再做 [`compute_sliding_window_limiter`] 的纯计算。
    pub fn tweak_speed_limiter_by_sliding_window(
        &self,
        downloader: Option<&str>,
        current: SpeedLimiter,
        now_ms: i64,
    ) -> SlidingWindowDynamicSpeedLimiter {
        let start = now_ms - SLIDING_WINDOW_MILLIS;
        let traffic = self.sink.query_traffic_overall(downloader, start, now_ms);
        let uploaded: i64 = traffic.iter().map(|d| d.data_overall_uploaded).sum();
        compute_sliding_window_limiter(
            uploaded,
            current.upload,
            self.settings.max_traffic_allowed_in_window_period,
            self.settings.traffic_sliding_capping_min_speed,
            self.settings.traffic_sliding_capping_max_speed,
            now_ms,
        )
    }
}

impl RuleModule for ActiveMonitoringModule {
    fn name(&self) -> &str {
        "Active Monitoring"
    }

    fn config_name(&self) -> &str {
        "active-monitoring"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        _torrent: &TorrentData,
        _peer: &PeerData,
        _ctx: &CheckContext,
    ) -> CheckResult {
        // 上游 ActiveMonitoringModule 没有 shouldBanPeer（非 RuleFeatureModule），
        // RunCheckModuleOrgan 第 51 行会跳过它；这里保持 pass()，绝不封禁。
        CheckResult::pass(self.config_name())
    }
}

use super::*;
