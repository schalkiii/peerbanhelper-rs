//! BtnNetwork 主体：握手、能力同步、规则/订阅记录上报与 IpQuery。

use super::*;
use super::pow::MAX_HISTORY_PAGES;

// ------------------------------------------------------------- 握手结果

/// `BtnNetwork.configBtnNetwork` 的结束状态（`configSuccess` / `configResult`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BtnConfigStatus {
    /// 尚未尝试（`reloadConfig` 后的初值，等价于 `configSuccess=false`）
    Pending,
    /// `configSuccess=true`
    Success { submit: bool },
    /// HTTP 非 2xx（上游 `BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST`）
    HttpError { status: u16, body: String },
    /// 协议版本不兼容：客户端实现版本低于 `min_protocol_version`
    ClientTooOld { implemented: i32, min: i32 },
    /// 协议版本不兼容：服务端 `max_protocol_version` 低于客户端实现版本
    ServerTooOld { implemented: i32, max: i32 },
    /// 其它异常（缺字段 / JSON 非法 / 传输失败，上游 `BTN_CONFIG_STATUS_EXCEPTION`）
    Exception { kind: String, message: String },
}

impl BtnConfigStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    /// 上游 `configResult` 的 `Lang` 键
    pub fn lang_key(&self) -> &'static str {
        match self {
            Self::Pending => LANG_BTN_CONFIGURE_SYNC_SERVER,
            Self::Success { submit } => {
                if *submit {
                    LANG_BTN_CONFIG_STATUS_SUCCESSFUL
                } else {
                    LANG_BTN_CONFIG_STATUS_SUCCESSFUL_READ_ONLY
                }
            }
            Self::HttpError { .. } => LANG_BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST,
            Self::ClientTooOld { .. } => LANG_BTN_PROTOCOL_VERSION_CLIENT,
            Self::ServerTooOld { .. } => LANG_BTN_PROTOCOL_VERSION_SERVER,
            Self::Exception { .. } => LANG_BTN_CONFIG_STATUS_EXCEPTION,
        }
    }
}

/// [`BtnNetwork::sync_due`] 的报告（每个已到期的 ability 一条）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BtnAbilitySync {
    pub key: String,
    pub kind: BtnAbilityKind,
    /// 规则是否发生变化
    pub updated: bool,
    pub last_status: bool,
}

// -------------------------------------------------------------- BtnNetwork

/// 上游 `btn/BtnNetwork`：配置端点握手 + abilities 调度 + PoW captcha + 本地缓存。
///
/// 本移植**不创建任何线程**（上游有 `ScheduledExecutorService` 与 `BackgroundTaskManager`）；
/// 重试与到期判定都在 [`Self::check_if_need_retry_config`] / [`Self::sync_due`] 里以可注入
/// 时钟的方式暴露，由应用层按自己的定时器驱动。
pub struct BtnNetwork {
    config: RwLock<BtnNetworkConfig>,
    http: Arc<dyn BtnHttpClient>,
    pub(crate) metadata: Arc<dyn BtnMetadataStore>,
    pow_endpoint: RwLock<Option<String>>,
    pub(crate) abilities: RwLock<Vec<BtnAbility>>,
    config_success: AtomicBool,
    config_result: RwLock<BtnConfigStatus>,
    /// 上游 `nextConfigAttemptTime`（毫秒）
    next_config_attempt_ms: AtomicI64,
    /// 允许列表更新后需要解封的封禁表（上游 `btnNetwork.getServer().getBanList()`）
    ban_list: Option<Arc<StdMutex<BanList>>>,
    /// 本次同步是否更新了允许列表（触发 [`BtnNetworkOnline::on_rule_update`]）
    allowlist_changed: AtomicBool,
    /// 上报类能力的数据源（上游 `HistoryService` 等 DAO 的只读抽象；缺省 ⇒ 空库语义）
    submit_source: Option<Arc<dyn BtnSubmitSource>>,
    /// 心跳 `multi_if` 模式的本机网卡地址提供者（上游 `NetworkInterface.getNetworkInterfaces()`；
    /// 缺省为空列表 ⇒ 与上游「零可用网卡」等价，状态置 false）
    local_ips_provider: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    /// `BtnAbilitySubmitHistory` 的 `tryLock`（防止上一轮提交未完成时重入）
    pub(crate) submit_history_lock: StdMutex<()>,
    /// `LegacyBtnAbilitySubmitBans.lastReport`（epoch 毫秒；初值为构造时刻，
    /// 对齐上游 `lastReport = OffsetDateTime.now()`）
    pub(crate) legacy_bans_last_report_ms: AtomicI64,
}

impl std::fmt::Debug for BtnNetwork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtnNetwork")
            .field("config", &self.config.read().ok().map(|c| c.clone()))
            .field(
                "config_success",
                &self.config_success.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

/// 跨线程共享的 [`BtnNetwork`] 句柄（BTN 未启用时为 `None`）。
///
/// `BtnNetwork` 在 BTN 工作线程内构造（阻塞式 HTTP 客户端要求），
/// Web 层的 `GET /api/peer/{ip}/btnQuery` 通过该句柄拿到只读入口
/// （对齐上游 `PBHPeerController` 的 `@Autowired BtnNetwork`）。
#[derive(Clone, Default)]
pub struct SharedBtnNetwork(Arc<StdMutex<Option<Arc<BtnNetwork>>>>);

impl SharedBtnNetwork {
    /// 供 BTN 工作线程在构造完成后写入（重复写入覆盖，便于线程重启）。
    pub fn set(&self, network: Arc<BtnNetwork>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(network);
        }
    }

    /// 当前句柄（未启用 / 未完成握手时为 `None`）。
    pub fn get(&self) -> Option<Arc<BtnNetwork>> {
        self.0.lock().ok().and_then(|slot| slot.clone())
    }

    /// `BtnAbilityIpQuery.query` 的只读入口（阻塞调用，调用方自行 `spawn_blocking`）。
    ///
    /// 返回 `None` = BTN 未启用；`Some(Ok(None))` = 未注册 `ip_query` 能力；
    /// `Some(Ok(Some(_)))` = 查询结果；`Some(Err(_))` = HTTP/认证失败。
    pub fn query_ip(&self, address: &str) -> Option<anyhow::Result<Option<IpQueryResult>>> {
        self.get().map(|network| network.query_ip(address))
    }
}

impl std::fmt::Debug for SharedBtnNetwork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedBtnNetwork")
            .field("attached", &self.get().is_some())
            .finish()
    }
}

impl BtnNetwork {
    pub fn new(
        config: BtnNetworkConfig,
        http: Arc<dyn BtnHttpClient>,
        metadata: Arc<dyn BtnMetadataStore>,
    ) -> Self {
        // 上游 `reloadConfig()`：`nextConfigAttemptTime = System.currentTimeMillis()`（立即尝试）
        let next = if config.is_active() {
            i64::MIN
        } else {
            i64::MAX
        };
        Self {
            config: RwLock::new(config),
            http,
            metadata,
            pow_endpoint: RwLock::new(None),
            abilities: RwLock::new(Vec::new()),
            config_success: AtomicBool::new(false),
            config_result: RwLock::new(BtnConfigStatus::Pending),
            next_config_attempt_ms: AtomicI64::new(next),
            ban_list: None,
            allowlist_changed: AtomicBool::new(false),
            submit_source: None,
            local_ips_provider: None,
            submit_history_lock: StdMutex::new(()),
            // 对齐上游 `LegacyBtnAbilitySubmitBans.lastReport = OffsetDateTime.now()`：
            // 初值取构造时刻，避免首轮把启动前的全部历史封禁重报一遍。
            legacy_bans_last_report_ms: AtomicI64::new(now_millis()),
        }
    }

    /// 绑定封禁表（[`DownloaderServer.getBanList()`]）；用于允许列表更新后的解封。
    pub fn with_ban_list(mut self, ban_list: Arc<StdMutex<BanList>>) -> Self {
        self.ban_list = Some(ban_list);
        self
    }

    /// 绑定上报数据源（[`BtnSubmitSource`]）；缺省 ⇒ 提交类能力按「空库」对待。
    pub fn with_submit_source(mut self, submit_source: Arc<dyn BtnSubmitSource>) -> Self {
        self.submit_source = Some(submit_source);
        self
    }

    /// 绑定心跳 `multi_if` 模式的本机网卡地址列表（上游 `NetworkInterface.getNetworkInterfaces()`；
    /// 平台无关实现由调用方提供，例如 `if-addrs` 或系统命令）。
    pub fn with_local_ips(mut self, provider: Arc<dyn Fn() -> Vec<String> + Send + Sync>) -> Self {
        self.local_ips_provider = Some(provider);
        self
    }

    // ------------------------------------------------------------ 配置

    pub fn config(&self) -> BtnNetworkConfig {
        self.config.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// 上游 `reloadConfig()`：重载主配置、清空状态、重置 abilities 与重试时刻。
    pub fn reload_config(&self, config: BtnNetworkConfig) {
        let next = if config.is_active() {
            now_millis()
        } else {
            i64::MAX
        };
        if let Ok(mut slot) = self.config.write() {
            *slot = config;
        }
        self.config_success.store(false, Ordering::Relaxed);
        if let Ok(mut slot) = self.config_result.write() {
            *slot = BtnConfigStatus::Pending;
        }
        self.reset_abilities();
        self.next_config_attempt_ms.store(next, Ordering::Relaxed);
    }

    /// 上游 `resetAbilities()`：`abilities.values().forEach(unload); abilities.clear();`
    pub fn reset_abilities(&self) {
        if let Ok(mut slot) = self.pow_endpoint.write() {
            *slot = None;
        }
        if let Ok(mut slot) = self.abilities.write() {
            slot.clear();
        }
    }

    pub fn config_success(&self) -> bool {
        self.config_success.load(Ordering::Relaxed)
    }

    pub fn config_result(&self) -> BtnConfigStatus {
        self.config_result
            .read()
            .map(|s| s.clone())
            .unwrap_or(BtnConfigStatus::Pending)
    }

    pub fn abilities(&self) -> Vec<BtnAbility> {
        self.abilities.read().map(|a| a.clone()).unwrap_or_default()
    }

    /// 上游 `checkIfNeedRetryConfig()`：未成功且已过重试时刻 ⇒ 重新握手。
    ///
    /// `enabled=false` 时等价于上游「调度器为 null」：只把 `configSuccess` 置回 false，
    /// **不产生任何网络请求**。
    pub fn check_if_need_retry_config(&self, module: &BtnNetworkOnline) -> bool {
        self.check_if_need_retry_config_at(module, now_millis())
    }

    /// [`Self::check_if_need_retry_config`] 的可注入时钟版本。
    pub fn check_if_need_retry_config_at(&self, module: &BtnNetworkOnline, now_ms: i64) -> bool {
        if !self.config().is_active() {
            // 上游 `checkIfNeedRetryConfig` 的 else 分支
            self.config_success.store(false, Ordering::Relaxed);
            return false;
        }
        if self.config_success.load(Ordering::Relaxed) {
            return true;
        }
        if now_ms <= self.next_config_attempt_ms.load(Ordering::Relaxed) {
            return false;
        }
        self.config_btn_network_at(module, now_ms)
    }

    /// 上游 `configBtnNetwork()`：GET `configUrl` → 协议版本校验 → 构造 abilities → `load()`。
    ///
    /// 任何失败都记录 [`BtnConfigStatus`] 并把重试时刻推到 `now + 600s`（上游
    /// `RETRY_PERIOD_SECONDS`）；**不会**触碰既有规则。
    pub fn config_btn_network(&self, module: &BtnNetworkOnline) -> bool {
        self.config_btn_network_at(module, now_millis())
    }

    /// [`Self::config_btn_network`] 的可注入时钟版本。
    pub fn config_btn_network_at(&self, module: &BtnNetworkOnline, now_ms: i64) -> bool {
        let config = self.config();
        if !config.is_active() {
            return false;
        }
        // 上游 `btn.allow-script-execute`：`BtnNetworkOnline.reloadConfig()` 与
        // `BtnNetwork.reloadConfig()` 读的是同一个键，握手时同步给模块。
        module.set_allow_script(config.allow_script_execute);
        debug!("{LANG_BTN_CONFIGURE_SYNC_SERVER}: {}", config.config_url);
        let request = self.request(&config.config_url);
        match self.http.execute(request) {
            Ok(response) if response.is_successful() => {
                match self.apply_config_response(module, &response.body) {
                    Ok(()) => {
                        self.config_success.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = self.config_result.write() {
                            *slot = BtnConfigStatus::Success {
                                submit: config.submit,
                            };
                        }
                        true
                    }
                    Err(status) => {
                        self.fail_config(status, now_ms);
                        false
                    }
                }
            }
            Ok(response) => {
                self.fail_config(
                    BtnConfigStatus::HttpError {
                        status: response.status,
                        body: response.body,
                    },
                    now_ms,
                );
                false
            }
            Err(e) => {
                self.fail_config(
                    BtnConfigStatus::Exception {
                        kind: "TransportError".to_string(),
                        message: e.to_string(),
                    },
                    now_ms,
                );
                false
            }
        }
    }

    fn fail_config(&self, status: BtnConfigStatus, now_ms: i64) {
        error!(
            "{LANG_BTN_CONFIG_FAILS}: {} ({})",
            status.lang_key(),
            status_message(&status)
        );
        self.config_success.store(false, Ordering::Relaxed);
        if let Ok(mut slot) = self.config_result.write() {
            *slot = status;
        }
        self.next_config_attempt_ms
            .store(now_ms + RETRY_PERIOD_SECONDS * 1000, Ordering::Relaxed);
    }

    /// 解析配置端点响应并构造 abilities（上游 `configBtnNetwork` 的 try 块主体）。
    fn apply_config_response(
        &self,
        module: &BtnNetworkOnline,
        body: &str,
    ) -> Result<(), BtnConfigStatus> {
        let json: BtnConfigResponse =
            serde_json::from_str(body).map_err(|e| BtnConfigStatus::Exception {
                kind: "JsonSyntaxException".to_string(),
                message: e.to_string(),
            })?;
        let missing_field = || BtnConfigStatus::Exception {
            kind: "IllegalStateException".to_string(),
            message: LANG_MISSING_VERSION_PROTOCOL_FIELD.to_string(),
        };
        let min = json.min_protocol_version.ok_or_else(missing_field)?;
        if BTN_PROTOCOL_IMPL_VERSION < min {
            return Err(BtnConfigStatus::ClientTooOld {
                implemented: BTN_PROTOCOL_IMPL_VERSION,
                min,
            });
        }
        let max = json.max_protocol_version.ok_or_else(missing_field)?;
        if BTN_PROTOCOL_IMPL_VERSION > max {
            return Err(BtnConfigStatus::ServerTooOld {
                implemented: BTN_PROTOCOL_IMPL_VERSION,
                max,
            });
        }
        // 上游 `useLegacyAbilities = min_protocol_version < 20`：遗留协议没有 ip_* 列表能力
        let use_legacy_abilities = min < 20;
        self.reset_abilities();
        if let Some(pow) = &json.proof_of_work_captcha {
            if let Ok(mut slot) = self.pow_endpoint.write() {
                *slot = Some(pow.endpoint.clone());
            }
        }
        let submit = self.config().submit;
        let mut abilities: Vec<BtnAbility> = Vec::new();
        // 上游 `configBtnNetwork` 的注册顺序：遗留/现代分支 → 公共能力。
        // 注意遗留分支的 `submit_bans` 是 `LegacyBtnAbilitySubmitBans`，现代是 `BtnAbilitySubmitBans`。
        let mut register = |key: &str, kind: BtnAbilityKind| {
            if let Some(spec) = json.ability.get(key) {
                let mut ability = BtnAbility::from_spec(key, spec);
                ability.kind = kind;
                abilities.push(ability);
            }
        };
        if use_legacy_abilities {
            if submit {
                register("submit_peers", BtnAbilityKind::LegacySubmitPeers);
                register("submit_bans", BtnAbilityKind::LegacySubmitBans);
            }
            register("rules", BtnAbilityKind::Rules);
        } else {
            if submit {
                register("submit_bans", BtnAbilityKind::SubmitBans);
                register("submit_swarm", BtnAbilityKind::SubmitSwarm);
            }
            register("ip_denylist", BtnAbilityKind::IpDenyList);
            register("ip_allowlist", BtnAbilityKind::IpAllowList);
            register("rule_peer_identity", BtnAbilityKind::Rules);
        }
        if submit {
            register("submit_histories", BtnAbilityKind::SubmitHistories);
        }
        register("reconfigure", BtnAbilityKind::Reconfigure);
        register("heartbeat", BtnAbilityKind::Heartbeat);
        register("ip_query", BtnAbilityKind::IpQuery);
        let now = now_millis();
        for ability in &mut abilities {
            ability.next_due_ms =
                now + pseudo_random(&ability.key, ability.random_initial_delay_ms);
        }
        if let Ok(mut slot) = self.abilities.write() {
            *slot = abilities;
        }
        // 上游 `abilities.values().forEach(a -> { try { a.load(); } catch ... })`
        self.load_abilities(module);
        Ok(())
    }

    /// 上游各 ability 的 `load()`：从 `metadataDao` 读回缓存 → 注入模块。
    ///
    /// 允许列表从缓存加载后同样要解封（上游 `BtnAbilityIPAllowList.load()` 调
    /// `unbanAllowedBannedPeers()`）。
    pub fn load_abilities(&self, module: &BtnNetworkOnline) {
        let kinds: Vec<BtnAbilityKind> = self.abilities().iter().map(|a| a.kind).collect();
        for kind in kinds {
            self.load_ability(module, kind);
        }
        self.flush_allowlist_unban(module);
    }

    fn load_ability(&self, module: &BtnNetworkOnline, kind: BtnAbilityKind) {
        match kind {
            BtnAbilityKind::Rules => {
                if let Some(cache) = self.metadata.get(CACHE_KEY_RULES) {
                    if let Err(e) = module.apply_ruleset_json(&cache) {
                        warn!(
                            "{LANG_UNABLE_LOAD_BTN_ABILITY}: {} - {e}",
                            kind.upstream_name()
                        );
                    } else {
                        debug!("{LANG_BTN_RULES_LOADED_FROM_CACHE}");
                    }
                }
            }
            BtnAbilityKind::IpAllowList => {
                let version = self.metadata.get(CACHE_KEY_IP_ALLOWLIST_VERSION);
                if let Some(value) = self.metadata.get(CACHE_KEY_IP_ALLOWLIST_VALUE) {
                    let rev = version.as_deref().unwrap_or(INITIAL_REV);
                    let loaded = module.apply_ip_allowlist_text(&value, rev);
                    self.allowlist_changed.store(true, Ordering::Relaxed);
                    debug!("[BTN AllowList] 从缓存加载 {loaded} 条，版本 {rev}");
                }
            }
            BtnAbilityKind::IpDenyList => {
                let version = self.metadata.get(CACHE_KEY_IP_DENYLIST_VERSION);
                if let Some(value) = self.metadata.get(CACHE_KEY_IP_DENYLIST_VALUE) {
                    let rev = version.as_deref().unwrap_or(INITIAL_REV);
                    let loaded = module.apply_ip_denylist_text(&value, rev);
                    debug!("[BTN DenyList] 从缓存加载 {loaded} 条，版本 {rev}");
                }
            }
            // 上报类能力只有孤游标存于 metadata（无本地规则缓存），无需回灌
            _ => {}
        }
    }

    // ------------------------------------------------------- 请求构造 / PoW

    /// 上游 `setupHttpClient()` 的拦截器逻辑本体：给任意请求附加统一头部
    /// （`User-Agent` / `Content-Type` / `BTN-AppID` / `BTN-AppSecret` / `X-BTN-AppID` /
    /// `X-BTN-AppSecret` / `Authentication: Bearer <appId>@<appSecret>`；匿名账户加
    /// `X-BTN-InstallationID`）。上报类 POST 载荷与配置 GET 请求都走这里。
    fn apply_auth(&self, request: BtnHttpRequest) -> BtnHttpRequest {
        let config = self.config();
        let mut request = request
            .header("User-Agent", BTN_USER_AGENT)
            .header("Content-Type", "application/json")
            .header(HEADER_BTN_APP_ID, config.app_id.clone())
            .header(HEADER_BTN_APP_SECRET, config.app_secret.clone())
            .header(HEADER_X_BTN_APP_ID, config.app_id.clone())
            .header(HEADER_X_BTN_APP_SECRET, config.app_secret.clone())
            .header(
                HEADER_AUTHENTICATION,
                format!("Bearer {}@{}", config.app_id, config.app_secret),
            );
        if config.is_anonymous() {
            request = request.header(HEADER_X_BTN_INSTALLATION_ID, config.installation_id.clone());
        }
        request
    }

    /// 带认证头的 `GET` 请求（配置端点等）。
    fn request(&self, url: &str) -> BtnHttpRequest {
        self.apply_auth(BtnHttpRequest::get(url))
    }

    /// 上游 `gatherAndSolveCaptchaBlocking(requestBuilder, type)`：
    /// GET `<powCaptchaEndpoint>?type=<type>` → 求解 → 追加 `X-BTN-PowID` / `X-BTN-PowSolution`。
    ///
    /// 未配置 PoW 端点、求解失败或算法不支持时**不追加任何头**（与上游 `return;` /
    /// `catch (Throwable)` 的分支一致）。
    pub fn gather_and_solve_captcha(
        &self,
        request: BtnHttpRequest,
        pow_type: &str,
    ) -> BtnHttpRequest {
        let endpoint = match self.pow_endpoint.read() {
            Ok(slot) => match slot.as_ref() {
                Some(endpoint) if !endpoint.trim().is_empty() => endpoint.clone(),
                _ => return request,
            },
            Err(_) => return request,
        };
        let url = format!("{endpoint}?type={pow_type}");
        let response = match self.http.execute(self.request(&url)) {
            Ok(response) => response,
            Err(e) => {
                error!("Unable to gather or solve PoW Captcha: {e}");
                return request;
            }
        };
        if !response.is_successful() {
            error!(
                "{LANG_BTN_POW_CAPTCHA_LOAD_FROM_REMOTE}: {} {}",
                response.status, response.body
            );
            return request;
        }
        let data: PowCaptchaData = match serde_json::from_str(&response.body) {
            Ok(data) => data,
            Err(e) => {
                error!("Unable to gather or solve PoW Captcha: {e}");
                return request;
            }
        };
        let challenge = match base64_decode(&data.challenge_base64) {
            Some(challenge) => challenge,
            None => {
                error!("Unable to gather or solve PoW Captcha: invalid base64 challenge");
                return request;
            }
        };
        debug!("{LANG_BTN_POW_CAPTCHA_COMPUTING}");
        let started = now_millis();
        match solve_pow(&challenge, data.difficulty_bits, &data.algorithm) {
            Some(nonce) => {
                debug!(
                    "{LANG_BTN_POW_CAPTCHA_COMPUTE_COMPLETED}: {} ms",
                    now_millis() - started
                );
                request
                    .header(HEADER_X_BTN_POW_ID, data.id.clone())
                    .header(HEADER_X_BTN_POW_SOLUTION, base64_encode(&nonce))
            }
            None => {
                warn!("Unable to gather or solve PoW Captcha: solver gave up");
                request
            }
        }
    }

    // ------------------------------------------------------------- 同步

    /// 一次完整同步（上游各 ability 的 `updateRule()` 依次执行）。
    ///
    /// 返回是否有规则集被更新；允许列表变化后会按上游 `unbanAllowedBannedPeers()` 解封。
    pub fn sync(&self, module: &BtnNetworkOnline) -> bool {
        if !self.config().is_active() {
            return false;
        }
        self.allowlist_changed.store(false, Ordering::Relaxed);
        let updated = module.sync_from_transport(self);
        self.flush_allowlist_unban(module);
        updated
    }

    /// 上游 `scheduleWithFixedDelay(this::updateRule, random(0, randomInitialDelay), interval, MS)`：
    /// 只执行「已到期」的 ability，并把下一次触发时刻推进 `interval` 毫秒。
    pub fn sync_due(&self, module: &BtnNetworkOnline, now_ms: i64) -> Vec<BtnAbilitySync> {
        if !self.config().is_active() {
            return Vec::new();
        }
        self.allowlist_changed.store(false, Ordering::Relaxed);
        let mut due: Vec<(usize, BtnAbilityKind, i64)> = Vec::new();
        if let Ok(slot) = self.abilities.read() {
            for (index, ability) in slot.iter().enumerate() {
                if ability.kind.is_implemented() && now_ms >= ability.next_due_ms {
                    due.push((index, ability.kind, ability.interval_ms));
                }
            }
        }
        let mut report = Vec::with_capacity(due.len());
        for (index, kind, interval) in due {
            let (updated, reported_status) = match self.run_ability(module, kind, index, now_ms) {
                AbilityOutcome::Reported { updated, status } => (updated, Some(status)),
                AbilityOutcome::Skipped => (false, None),
            };
            let last_status = if let Ok(mut slot) = self.abilities.write() {
                // `reconfigure` 可能已重建 ability 列表（`reset_abilities`），此时下标会串位：
                // 按 kind 重新定位，找不到就跳过本轮记账（上游用对象引用，不存在该问题）。
                if let Some(ability) = slot.iter_mut().find(|ability| ability.kind == kind) {
                    if let Some(status) = reported_status {
                        ability.last_status = status;
                        ability.last_status_at_ms = now_ms;
                    }
                    // 上游 fixed-delay：下一次触发 = 本次完成时刻 + interval
                    ability.next_due_ms = now_ms + interval;
                    ability.last_status
                } else {
                    true
                }
            } else {
                true
            };
            report.push(BtnAbilitySync {
                key: kind.json_key().to_string(),
                kind,
                updated,
                last_status,
            });
        }
        self.flush_allowlist_unban(module);
        report
    }

    /// 分发一个到期能力到唯一的执行入口（上游各 ability 的 `updateRule` / `submit` / 心跳回调）。
    fn run_ability(
        &self,
        module: &BtnNetworkOnline,
        kind: BtnAbilityKind,
        index: usize,
        now_ms: i64,
    ) -> AbilityOutcome {
        match kind {
            BtnAbilityKind::Rules => AbilityOutcome::Reported {
                updated: self.update_rules(module),
                status: true,
            },
            BtnAbilityKind::IpAllowList => {
                let rev = module.ip_list_version(true);
                AbilityOutcome::Reported {
                    updated: self.update_ip_list(module, BtnIpAbility::AllowList, &rev),
                    status: true,
                }
            }
            BtnAbilityKind::IpDenyList => {
                let rev = module.ip_list_version(false);
                AbilityOutcome::Reported {
                    updated: self.update_ip_list(module, BtnIpAbility::DenyList, &rev),
                    status: true,
                }
            }
            BtnAbilityKind::SubmitBans => self.run_submit_bans(index),
            BtnAbilityKind::SubmitSwarm => self.run_submit_swarm(index),
            BtnAbilityKind::SubmitHistories => self.run_submit_histories(index),
            BtnAbilityKind::LegacySubmitPeers => self.run_legacy_submit_peers(index),
            BtnAbilityKind::LegacySubmitBans => self.run_legacy_submit_bans(index),
            BtnAbilityKind::Heartbeat => {
                let status = self.run_heartbeat(index, now_ms);
                AbilityOutcome::Reported {
                    updated: false,
                    status,
                }
            }
            BtnAbilityKind::Reconfigure => {
                let status = self.run_reconfigure(module, index, now_ms);
                AbilityOutcome::Reported {
                    updated: false,
                    status,
                }
            }
            // `IpQuery` 不注册调度器（`is_scheduled() == false`），不会进入 due
            BtnAbilityKind::IpQuery | BtnAbilityKind::Other => AbilityOutcome::Reported {
                updated: false,
                status: true,
            },
        }
    }

    fn ability_at(&self, index: usize) -> Option<BtnAbility> {
        self.abilities
            .read()
            .ok()
            .and_then(|slot| slot.get(index).cloned())
    }

    fn set_last_status(&self, index: usize, status: bool, now_ms: i64) {
        if let Ok(mut slot) = self.abilities.write() {
            if let Some(ability) = slot.get_mut(index) {
                ability.last_status = status;
                ability.last_status_at_ms = now_ms;
            }
        }
    }

    fn metadata_i64(&self, key: &str, default: i64) -> i64 {
        self.metadata
            .get(key)
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(default)
    }

    // ---------------------------------------------------------- submit_bans

    fn run_submit_bans(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_bans：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let mut cursor = self.metadata_i64(METADATA_KEY_BANS_CURSOR, 0);
        let mut total = 0usize;
        loop {
            let rows = source.batch_ban_history(cursor, 100);
            if rows.is_empty() {
                break;
            }
            let ping = BtnBanPing {
                bans: rows.iter().map(|row| row.clone().into_ping()).collect(),
            };
            if let Err(error) = self.post_gzip_report(&ability, &ping) {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                return AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                };
            }
            // 页提交成功后游标推进到本页最大 id（上游 `historyEntities.getLast().getId()`）
            cursor = rows.iter().map(|row| row.id).max().unwrap_or(cursor);
            self.metadata
                .set(METADATA_KEY_BANS_CURSOR, &cursor.to_string());
            total += rows.len();
            // 上游 `page.hasNext()`：本页不满 ⇒ 结束
            if rows.len() < 100 {
                break;
            }
        }
        debug!("BTN submit_bans：已上报 {total} 条");
        AbilityOutcome::Reported {
            updated: total > 0,
            status: true,
        }
    }

    // ---------------------------------------------------------- submit_swarm

    /// 上游 `getMemCursor`：`"<lastTimeSeen>,<id>"` 二元游标。
    fn swarm_cursor(&self) -> (i64, i64) {
        match self.metadata.get(METADATA_KEY_SWARM_CURSOR) {
            Some(value) => {
                let parts: Vec<&str> = value.splitn(2, ',').collect();
                let last = parts
                    .first()
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
                let id = parts
                    .get(1)
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
                (last, id)
            }
            None => (0, 0),
        }
    }

    fn run_submit_swarm(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_swarm：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let (mut last_seen, mut id) = self.swarm_cursor();
        let mut total = 0usize;
        loop {
            let rows = source.batch_swarm_history(last_seen, id, 1000);
            if rows.is_empty() {
                break;
            }
            let ping = BtnSwarmPeerPing {
                swarms: rows.iter().map(|row| row.clone().into_ping()).collect(),
            };
            if let Err(error) = self.post_gzip_report(&ability, &ping) {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                return AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                };
            }
            // 排序保证最后一条即最大（上游 `getLast()`）
            last_seen = rows
                .last()
                .map(|row| row.last_time_seen_ms)
                .unwrap_or(last_seen);
            id = rows.last().map(|row| row.id).unwrap_or(id);
            self.metadata
                .set(METADATA_KEY_SWARM_CURSOR, &format!("{last_seen},{id}"));
            total += rows.len();
            if rows.len() < 1000 {
                break;
            }
        }
        debug!("BTN submit_swarm：已上报 {total} 条");
        AbilityOutcome::Reported {
            updated: total > 0,
            status: true,
        }
    }

    // ------------------------------------------------------- submit_histories

    /// 上游 `getLastSubmitAtTimestamp`：读取游标并做 30 天饱和（过期/缺失 ⇒ 重设为当前时刻）。
    fn history_timestamp(&self) -> i64 {
        let now = now_millis();
        let valid = self
            .metadata
            .get(METADATA_KEY_HISTORY_TIMESTAMP)
            .and_then(|value| value.parse::<i64>().ok())
            .is_some_and(|stored| stored >= now - 30 * 24 * 60 * 60 * 1000);
        if !valid {
            self.metadata
                .set(METADATA_KEY_HISTORY_TIMESTAMP, &now.to_string());
            return now;
        }
        self.metadata_i64(METADATA_KEY_HISTORY_TIMESTAMP, now)
    }

    fn run_submit_histories(&self, index: usize) -> AbilityOutcome {
        // 上游 `lock.tryLock()`：上一轮未结束则跳过本次，且不更新 lastStatus
        let Ok(_guard) = self.submit_history_lock.try_lock() else {
            debug!("BTN submit_histories：上一轮仍在提交，跳过本次");
            return AbilityOutcome::Skipped;
        };
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_histories：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let now = now_millis();
        let mut last_submit_at = self.history_timestamp();
        let mut previous_cursor = last_submit_at;
        let mut total = 0usize;
        let mut pages = 0usize;
        loop {
            let rows = source.batch_peer_history(last_submit_at, 5000);
            if rows.is_empty() {
                break;
            }
            let ping = LegacyBtnPeerHistoryPing {
                populate_time: now,
                peers: rows.iter().map(|row| row.clone().into_ping()).collect(),
            };
            if let Err(error) = self.post_gzip_report(&ability, &ping) {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                return AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                };
            }
            total += rows.len();
            let mut last_record_at = rows
                .last()
                .map(|row| row.last_time_seen_ms)
                .unwrap_or(last_submit_at);
            if last_record_at >= last_submit_at {
                // 与上游一致：游标与最新记录相同时 +1，防止死循环
                last_record_at = if last_record_at == last_submit_at {
                    last_record_at + 1
                } else {
                    last_record_at
                };
                last_submit_at = last_record_at;
            } else {
                last_submit_at = now_millis();
            }
            self.metadata
                .set(METADATA_KEY_HISTORY_TIMESTAMP, &last_submit_at.to_string());
            if rows.len() < 5000 {
                break;
            }
            // 防御：数据源若持续回带同一批行（例如 `last_time_seen` 为 NULL 的记录，
            // 上游在该处会 NPE 并终止），游标无法推进会造成无限上报。这里限制单轮页数
            // 并在游标未推进时退出，避免占住 `submit_history_lock` 与网络。
            pages += 1;
            if last_submit_at <= previous_cursor || pages >= MAX_HISTORY_PAGES {
                warn!("BTN submit_histories：游标未推进（{previous_cursor} → {last_submit_at}），本轮提前结束");
                break;
            }
            previous_cursor = last_submit_at;
        }
        debug!("BTN submit_histories：已上报 {total} 条");
        AbilityOutcome::Reported {
            updated: total > 0,
            status: true,
        }
    }

    // ------------------------------------------------------- legacy 上报

    /// 遗留 `submit_peers`：一次性全量 live peer 快照（无分页、无游标）。
    fn run_legacy_submit_peers(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_peers：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let peers: Vec<LegacyBtnPeer> = source
            .legacy_peer_snapshot()
            .iter()
            .map(|row| row.clone().into_peer())
            .collect();
        if peers.is_empty() {
            // 上游：空列表不发请求，`setLastStatus(true, BTN_LAST_REPORT_EMPTY)`
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        }
        let ping = LegacyBtnPeerPing {
            populate_time: now_millis(),
            peers,
        };
        match self.post_gzip_report(&ability, &ping) {
            Ok(()) => AbilityOutcome::Reported {
                updated: true,
                status: true,
            },
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                }
            }
        }
    }

    /// 遗留 `submit_bans`：上报 `banAt > lastReport` 的新增封禁
    /// （上游按 `BanMetadata.getBanAt` 过滤，`lastReport` 为内存态）。
    fn run_legacy_submit_bans(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN legacy submit_bans：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let now = now_millis();
        let last_report = self.legacy_bans_last_report_ms.load(Ordering::Relaxed);
        let rows: Vec<BtnLegacyBanRow> = source
            .legacy_ban_snapshot()
            .into_iter()
            .filter(|row| row.ban_at_ms > last_report)
            .collect();
        if rows.is_empty() {
            self.legacy_bans_last_report_ms
                .store(now, Ordering::Relaxed);
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        }
        let ping = LegacyBtnBanPing {
            populate_time: now,
            bans: rows.iter().map(|row| row.clone().into_ping()).collect(),
        };
        match self.post_gzip_report(&ability, &ping) {
            Ok(()) => {
                let last_ban_at = rows.iter().map(|row| row.ban_at_ms).max().unwrap_or(now);
                self.legacy_bans_last_report_ms
                    .store(last_ban_at, Ordering::Relaxed);
                AbilityOutcome::Reported {
                    updated: true,
                    status: true,
                }
            }
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now);
                AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                }
            }
        }
    }

    // ------------------------------------------------------------- heartbeat

    fn run_heartbeat(&self, index: usize, now_ms: i64) -> bool {
        let Some(ability) = self.ability_at(index) else {
            return false;
        };
        let ok = if ability.multi_if {
            // 上游 `NetworkInterface.getNetworkInterfaces()`；本移植由注入方提供
            let ips: Vec<String> = self
                .local_ips_provider
                .as_ref()
                .map(|provider| provider())
                .unwrap_or_default();
            if ips.is_empty() {
                return false;
            }
            // 上游并发对每个地址各发一次（不能短路）；任一成功即整体成功
            let mut any_success = false;
            for ip in &ips {
                if self.send_heartbeat(&ability, ip) {
                    any_success = true;
                }
            }
            any_success
        } else {
            self.send_heartbeat(&ability, "default")
        };
        self.set_last_status(index, ok, now_ms);
        ok
    }

    /// 一次心跳（上游 `sendHeartBeat`）：`POST {"ifaddr":"<if>|default"}`，解析响应的
    /// `external_ip`；默认接口用**无认证**的裸 client（上游 `getHttpUtil()`），
    /// `multi_if` 的每个地址用带认证的 client。
    fn send_heartbeat(&self, ability: &BtnAbility, ifaddr: &str) -> bool {
        let body = serde_json::to_vec(&serde_json::json!({ "ifaddr": ifaddr })).unwrap_or_default();
        let mut request = BtnHttpRequest::post(&ability.endpoint, body)
            .header("User-Agent", BTN_USER_AGENT)
            .header("Content-Type", "application/json");
        if ifaddr != "default" {
            request = self.apply_auth(request);
        }
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, "heartbeat");
        }
        match self.http.execute(request) {
            Ok(response) if response.is_successful() => {
                let external_ip = serde_json::from_str::<BtnHeartbeatResponse>(&response.body)
                    .ok()
                    .and_then(|data| data.external_ip);
                match external_ip {
                    Some(ip) => debug!("BTN heartbeat `{ifaddr}` → 外部 IP: {ip}"),
                    None => debug!("BTN heartbeat `{ifaddr}` → 服务器未返回 external_ip"),
                }
                true
            }
            Ok(response) => {
                warn!(
                    "{LANG_BTN_REQUEST_FAILS}: heartbeat `{ifaddr}` → HTTP {} - {}",
                    response.status, response.body
                );
                false
            }
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: heartbeat `{ifaddr}` → {error}");
                false
            }
        }
    }

    // ---------------------------------------------------------- reconfigure

    fn run_reconfigure(&self, module: &BtnNetworkOnline, index: usize, now_ms: i64) -> bool {
        let Some(ability) = self.ability_at(index) else {
            return false;
        };
        let Some(local_version) = ability.reconfigure_version.as_deref() else {
            return true;
        };
        let response = match self.http.execute(self.request(&self.config().config_url)) {
            Ok(response) => response,
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: reconfigure → {error}");
                self.set_last_status(index, false, now_ms);
                return false;
            }
        };
        if !response.is_successful() {
            warn!(
                "{LANG_BTN_REQUEST_FAILS}: reconfigure → HTTP {} - {}",
                response.status, response.body
            );
            self.set_last_status(index, false, now_ms);
            return false;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&response.body) else {
            warn!("{LANG_BTN_REQUEST_FAILS}: reconfigure → 响应不是合法 JSON");
            self.set_last_status(index, false, now_ms);
            return false;
        };
        let Some(remote) = json
            .pointer("/ability/reconfigure/version")
            .and_then(|value| value.as_str())
        else {
            // 服务端已移除 reconfigure ⇒ 视为禁用（上游 `BTN_RECONFIGURE_DISABLED_BY_SERVER`）
            return true;
        };
        if remote == local_version {
            return true;
        }
        info!("BTN reconfigure：服务端 {remote} != 本地 {local_version}，重新握手");
        let ok = self.config_btn_network_at(module, now_ms);
        if !ok {
            self.set_last_status(index, false, now_ms);
        }
        ok
    }

    // ------------------------------------------------------------ ip_query

    /// 上游 `BtnAbilityIpQuery.query(address)`：`GET endpoint?ip=<urlencoded>`（认证头 + 可选
    /// PoW `ip_query`）。未配置该能力时返回 `Ok(None)`；HTTP 失败抛出 `anyhow::Error`。
    pub fn query_ip(&self, address: &str) -> anyhow::Result<Option<IpQueryResult>> {
        let Some(ability) = self
            .abilities()
            .into_iter()
            .find(|ability| ability.kind == BtnAbilityKind::IpQuery)
        else {
            return Ok(None);
        };
        let url = append_url(&ability.endpoint, &[("ip", &url_encode_query(address))]);
        let mut request = self.request(&url);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, "ip_query");
        }
        let response = self.http.execute(request)?;
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        Ok(Some(serde_json::from_str(&response.body)?))
    }

    /// 上游 `createGzipRequestBody` + 认证头 + 可选 PoW：payload → gzip → POST。
    /// 非 2xx 视为失败（上游抛 `IllegalStateException`，游标不推进、状态置 false）。
    fn post_gzip_report(
        &self,
        ability: &BtnAbility,
        payload: &impl Serialize,
    ) -> anyhow::Result<()> {
        let json = serde_json::to_vec(payload)?;
        let body = gzip_bytes(&json)?;
        let request = BtnHttpRequest::post(&ability.endpoint, body)
            .header("User-Agent", BTN_USER_AGENT)
            .header("Content-Type", "application/json")
            .header("Content-Encoding", "gzip");
        let mut request = self.apply_auth(request);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, ability.kind.pow_type());
        }
        let response = self.http.execute(request)?;
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        Ok(())
    }

    fn update_rules(&self, module: &BtnNetworkOnline) -> bool {
        let rev = module
            .ruleset_version()
            .unwrap_or_else(|| INITIAL_REV.to_string());
        match self.fetch_ruleset(&rev) {
            Ok(Some(ruleset)) => match module.apply_ruleset(&ruleset) {
                Ok(()) => {
                    info!(
                        "{LANG_BTN_UPDATE_RULES_SUCCESSES}: {}",
                        module.ruleset_version().unwrap_or_default()
                    );
                    true
                }
                Err(e) => {
                    warn!("{LANG_BTN_REQUEST_FAILS}: {e}");
                    false
                }
            },
            Ok(None) => false,
            Err(e) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {e}");
                false
            }
        }
    }

    fn update_ip_list(&self, module: &BtnNetworkOnline, kind: BtnIpAbility, rev: &str) -> bool {
        let allow = kind == BtnIpAbility::AllowList;
        match self.fetch_ip_list(kind, rev) {
            Ok(Some((text, version))) => {
                let loaded = if allow {
                    module.apply_ip_allowlist_text(&text, &version)
                } else {
                    module.apply_ip_denylist_text(&text, &version)
                };
                let lang = if allow {
                    LANG_BTN_ABILITY_ALLOWLIST_LOADED_FROM_REMOTE
                } else {
                    LANG_BTN_ABILITY_DENYLIST_LOADED_FROM_REMOTE
                };
                info!("{lang}: {version}, {loaded}");
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {e}");
                false
            }
        }
    }

    /// 上游允许列表更新后（`@Subscribe onRuleUpdate` / `finally { unbanAllowedBannedPeers(); }`）
    /// 解封命中允许列表的地址。
    fn flush_allowlist_unban(&self, module: &BtnNetworkOnline) {
        if !self.allowlist_changed.swap(false, Ordering::Relaxed) {
            return;
        }
        if let Some(ban_list) = &self.ban_list {
            module.on_rule_update(ban_list);
        }
    }

    /// 上游 `URLUtil.appendUrl(endpoint, Map.of("rev", version))`。
    pub(crate) fn rev_url(&self, endpoint: &str, rev: &str) -> String {
        if endpoint.contains('?') {
            format!("{endpoint}&rev={rev}")
        } else {
            format!("{endpoint}?rev={rev}")
        }
    }
}

impl BtnTransport for BtnNetwork {
    /// 上游 `BtnAbilityRules.updateRule`：GET `<endpoint>?rev=<version>`；
    /// 204 ⇒ 无变化；非 2xx ⇒ 失败；2xx ⇒ 解析规则集并写入 `metadataDao` 缓存。
    fn fetch_ruleset(&self, rev: &str) -> anyhow::Result<Option<BtnRuleset>> {
        let Some(ability) = self
            .abilities()
            .into_iter()
            .find(|a| a.kind == BtnAbilityKind::Rules)
        else {
            // 上游：未构造该 ability ⇒ 不存在这次请求
            return Ok(None);
        };
        let url = self.rev_url(&ability.endpoint, rev);
        let mut request = self.request(&url);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, ability.kind.pow_type());
        }
        let response = self.http.execute(request)?;
        if response.is_no_content() {
            return Ok(None);
        }
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        let ruleset: BtnRuleset = serde_json::from_str(&response.body)?;
        self.metadata.set(CACHE_KEY_RULES, &response.body);
        Ok(Some(ruleset))
    }

    /// 上游 `BtnAbilityIP*List.updateRule`：同上，额外取 `X-BTN-ContentVersion` 头。
    fn fetch_ip_list(
        &self,
        kind: BtnIpAbility,
        rev: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        let wanted = match kind {
            BtnIpAbility::AllowList => BtnAbilityKind::IpAllowList,
            BtnIpAbility::DenyList => BtnAbilityKind::IpDenyList,
        };
        let Some(ability) = self.abilities().into_iter().find(|a| a.kind == wanted) else {
            return Ok(None);
        };
        let url = self.rev_url(&ability.endpoint, rev);
        let mut request = self.request(&url);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, ability.kind.pow_type());
        }
        let response = self.http.execute(request)?;
        if response.is_no_content() {
            return Ok(None);
        }
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        // 上游 `response.header("X-BTN-ContentVersion", "unknown")`
        let version = response
            .header(HEADER_X_BTN_CONTENT_VERSION)
            .unwrap_or("unknown")
            .to_string();
        let (version_key, value_key) = match kind {
            BtnIpAbility::AllowList => {
                (CACHE_KEY_IP_ALLOWLIST_VERSION, CACHE_KEY_IP_ALLOWLIST_VALUE)
            }
            BtnIpAbility::DenyList => (CACHE_KEY_IP_DENYLIST_VERSION, CACHE_KEY_IP_DENYLIST_VALUE),
        };
        self.metadata.set(version_key, &version);
        self.metadata.set(value_key, &response.body);
        if kind == BtnIpAbility::AllowList {
            self.allowlist_changed.store(true, Ordering::Relaxed);
        }
        Ok(Some((response.body, version)))
    }
}

enum AbilityOutcome {
    /// 本轮已执行：`updated` 是否有可上报变化；`status` 即上游 `lastStatus`。
    Reported { updated: bool, status: bool },
    /// 本轮被跳过（如 `tryLock` 未取到）：lastStatus 保持不变。
    Skipped,
}

/// 上游 `okio.GzipSink`：把载荷压缩成 gzip 字节（请求带 `Content-Encoding: gzip`）。
fn status_message(status: &BtnConfigStatus) -> String {
    match status {
        BtnConfigStatus::Pending => "pending".to_string(),
        BtnConfigStatus::Success { submit } => format!("submit={submit}"),
        BtnConfigStatus::HttpError { status, body } => format!("{status} - {body}"),
        BtnConfigStatus::ClientTooOld { implemented, min } => {
            format!("client {implemented} < min {min}")
        }
        BtnConfigStatus::ServerTooOld { implemented, max } => {
            format!("client {implemented} > max {max}")
        }
        BtnConfigStatus::Exception { kind, message } => format!("{kind}: {message}"),
    }
}
