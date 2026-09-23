// === INTEGRATION SNIPPET (applied by main agent) ===
//
// 【1】`pbh/src/default-config.yml`：在 `profile.module:` 段内新增
//      （对齐上游 `profile.yml` 第 264-267 行；`enabled: true` 只表示本模块参与判定，
//       真正联网还要求主配置 `config.yml` 的 `btn.enabled: true`）：
//
// ```yaml
//     btn:
//       enabled: true
//       # 封禁时间，单位：毫秒，使用 default 则跟随全局设置
//       ban-duration: 259200000
// ```
//
// 【2】`pbh-core/src/config.rs`：`impl ProfileConfig::build_pipeline_with_geo` 内按上游
//      `PeerBanHelper.registerModules()` 的顺序（`AutoRangeBan` 之后、`IPBlackRuleList` 之前）
//      实例化（并 `use crate::modules::BtnNetworkOnline;`）：
//
// ```rust
// // 对齐上游 registerModules：AutoRangeBan -> BtnNetworkOnline -> IPBlackRuleList
// if let Some(cfg) = &self.module.btn {
//     if enabled_or_disabled(&cfg.enabled) {
//         pipeline.add_module(Box::new(BtnNetworkOnline::new(cfg.ban_duration_ms)));
//     }
// }
// ```
//
// 【3】应用层（`pbh/src/main.rs`）：仅当 `config.yml` 的 `btn.enabled: true` 且向 BTN 服务器
//      握手成功（拿到 abilities 列表）后才注入规则；**不注入时模块恒返回 `pass`、绝不封禁**：
//
// ```rust
// if let Some(btn) = pipeline.module_as::<BtnNetworkOnline>("btn") {
//     // 对齐 BtnAbilityRules.updateRule：HTTP 204 表示无变化，此时不要调用
//     btn.apply_ruleset_json(&ruleset_json)?;
//     // 对齐 BtnAbilityIPAllowList / BtnAbilityIPDenyList（规则文本 + X-BTN-ContentVersion）
//     btn.apply_ip_allowlist_text(&allow_list_text, &allow_list_version);
//     btn.apply_ip_denylist_text(&deny_list_text, &deny_list_version);
//     // 规则更新后（对齐 @Subscribe onRuleUpdate(BtnRuleUpdateEvent)）：
//     // btn.on_rule_update(&pipeline.ban_list);
// }
// ```
// === END INTEGRATION SNIPPET ===

//! BTN 网络在线规则（`btn`），忠实复刻上游 `module/impl/rule/BtnNetworkOnline`。
//!
//! 数据来源：BTN 服务器下发的规则集（`BtnRuleset` → `BtnRulesetParsed`），包含
//! peer-id / client-name / IP / port / script 五类规则；此外还有现代协议的两个 IP 列表能力
//! （`BtnAbilityIPAllowList` 白名单 → `SKIP`，`BtnAbilityIPDenyList` 黑名单 → `BAN`）。
//!
//! 判定顺序（逐字对齐上游 `BtnNetworkOnline.shouldBanPeer`）：
//! 1. `btnNetwork == null`（未配置/未初始化 BTN 客户端）→ 不封禁；
//! 2. `checkShouldSkip`：IP 白名单命中 → `SKIP`（ban-duration 0）；
//! 3. `checkScript`：脚本规则（`allow-script-execute`，见下「未移植」）；
//! 4. `checkShouldBanModern`：IP 黑名单命中 → `BAN`；
//! 5. `checkShouldBanLegacy`：规则集按 peer-id → client-name → IP → port 顺序判定；
//!    握手中的 peer 直接返回 `handshaking()`；任一 `SKIP` 立即返回；否则取**最后一个** `BAN`
//!    （对齐 `RuleParser.matchRule` 的「TRUE 可被后续覆盖、FALSE 最高优先」语义）；
//! 6. 全部未命中 → `pass()`。
//!
//! 传输层（本模块自身**不发起任何网络请求**，只提供注入入口）：
//! [`BtnTransport`] 的最小抽象 + [`BtnNetworkOnline::sync_from_transport`] 的「拉取 → 注入」
//! 流程与优雅降级语义由本模块保留；真正的 HTTP/握手/abilities/PoW 实现见
//! [`crate::btn_transport::BtnNetwork`]（它实现了 [`BtnTransport`]，可直接交给
//! [`BtnNetworkOnline::sync_from_transport`]）。
//!
//! BTN 脚本规则（`script` 类别）：上游用 `ScriptEngineManager` + AviatorScript 编译/执行，
//! 本移植用 **rhai**（与 [`crate::modules::expression_engine::ExpressionEngine`] 同样的引擎
//! 构造与返回值语义，见 `docs/expression-engine-migration.md`）。上游默认
//! `btn.allow-script-execute: false` ⇒ 不编译、不执行，与本移植默认行为一致。
//!
//! 未移植：
//! - 上游 `btn/BtnNetwork` 的 submit_* 能力（需要 pbh-core 不持有的 DAO，见
//!   [`crate::btn_transport`] 模块文档）；
//! - `BtnRuleUpdateEvent` 事件总线（[`BtnNetworkOnline::on_rule_update`] 保留等价逻辑，
//!   由传输层在允许列表更新后调用）；
//! - `ModuleMatchCache` 判定缓存（本移植每轮重新判定，结果等价）；
//! - `LegacyBtnExceptionRuleParsed` 例外规则：上游 v9.5.1 中 `checkPeerIdRuleException` 等
//!   私有方法已无任何调用点（死代码），故不移植。
//!
//! 与上游的唯一有意差异：`btnNetwork == null` 时上游返回 `BTN_MANAGER_NOT_INITIALIZED`
//! （同样是 `NO_ACTION`、同样绝不封禁，仅 `status`/文案不同），本移植统一返回 `pass()`
//! （`status: "pass"`）；上游结果形态保留在 [`btn_manager_not_initialized_result`] 中以便对照。

use crate::avscript::ScriptDownloader;
use crate::banlist::BanList;
use crate::i18n::{Param, TranslationComponent};
use crate::iputil::parse_addr;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, PeerAction, RuleModule};
use crate::rule::{Matcher, RuleSet};
use rhai::Engine;
use rhai::Scope;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// `module.btn.ban-duration` 的上游默认值：3 天（ms）。
pub const BTN_BAN_DURATION_MS: i64 = 259_200_000;

/// 对齐上游 `btn/BtnRuleset`（BTN 服务器下发的规则集 JSON DTO）。
///
/// 注意：上游用 `HashMap`，类别遍历顺序不确定；本实现用 `BTreeMap`（按类别名有序，
/// 使「命中多个类别时上报哪一个」可复现，结果集合本身不变）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnRuleset {
    #[serde(default)]
    pub version: Option<String>,
    /// `peer_id`：类别 → 规则 JSON 文本列表
    #[serde(rename = "peer_id", default)]
    pub peer_id_rules: BTreeMap<String, Vec<String>>,
    /// `client_name`：类别 → 规则 JSON 文本列表
    #[serde(rename = "client_name", default)]
    pub client_name_rules: BTreeMap<String, Vec<String>>,
    /// `ip`：类别 → IP/CIDR 列表
    #[serde(rename = "ip", default)]
    pub ip_rules: BTreeMap<String, Vec<String>>,
    /// `port`：类别 → 端口号列表
    #[serde(rename = "port", default)]
    pub port_rules: BTreeMap<String, Vec<i32>>,
    /// `script`：脚本名 → 脚本内容（脚本引擎未移植，见文件头）
    #[serde(rename = "script", default)]
    pub script_rules: BTreeMap<String, String>,
}

impl BtnRuleset {
    /// 对齐上游 `BtnAbilityRules.updateRule`：版本为空时用 `"initial"` 作为 rev。
    pub fn version_or_initial(&self) -> String {
        match self.version.as_deref() {
            Some(v) if !v.is_empty() => v.to_string(),
            _ => "initial".to_string(),
        }
    }
}

/// BTN 的 IP 匹配器（含备注），对齐上游 `util/rule/matcher/IPMatcher`：
/// 规则集内的 `ip` 类别与现代协议的允许/拒绝列表都基于它。
#[derive(Debug, Clone, Default)]
pub struct BtnIpList {
    /// 对齐上游 `setData(ruleName, ...)`：`"Empty IP Allowlist"` / `"BTN DenyList (Remote)"` 等
    pub rule_name: String,
    /// 对齐 `X-BTN-ContentVersion` / 规则集 version；未知时为 `"initial"`（对齐上游 `ruleVersion` 初值）
    pub version: String,
    entries: Vec<crate::modules::RuleListEntry>,
}

impl BtnIpList {
    /// 由规则集的 `ip` 类别（纯 IP/CIDR 列表）构造，对齐上游
    /// `BtnRulesetParsed.parseIPRule` 的 `tries.add(ip)`（无值 → 备注为空）。
    pub fn from_cidrs(rule_name: &str, version: &str, items: &[String]) -> Self {
        let mut entries = Vec::with_capacity(items.len());
        for raw in items {
            match crate::iputil::parse_net(raw) {
                Some(net) => entries.push(crate::modules::RuleListEntry {
                    net,
                    comment: String::new(),
                }),
                // 上游对不可解析项会抛异常并中止整份规则集加载；此处跳过（不会误命中，行为等价）
                None => tracing::warn!("BTN 规则集包含不可解析的 IP 规则 `{raw}`，已跳过"),
            }
        }
        Self {
            rule_name: rule_name.to_string(),
            version: version.to_string(),
            entries,
        }
    }

    /// 由允许/拒绝列表的规则文本构造，对齐上游 `BtnAbilityIP*List.stringToIPList`
    /// （逐行解析、`#` 注释累积、DAT/eMule 行、`level >= 128` 丢弃、行内注释）。
    ///
    /// 复用本 crate 的 [`crate::modules::ip_rule_list::parse_rule_list`]（与上游 `parseRuleLine` 一致），
    /// 返回 `(解析后的匹配器, 上游口径的加载行数)`。
    pub fn from_rule_text(rule_name: &str, version: &str, text: &str) -> (Self, usize) {
        let (entries, loaded) = crate::modules::ip_rule_list::parse_rule_list(text);
        (
            Self {
                rule_name: rule_name.to_string(),
                version: version.to_string(),
                entries,
            },
            loaded,
        )
    }

    /// 最长前缀匹配，返回命中规则的备注（上游 `IPMatcher.match0` 的
    /// `ips.elementsContaining(ip)` → `new TranslationComponent(node.getValue())`）。
    ///
    /// 地址不可解析时返回 `None`（上游 `getIPAddress` 返回 null → `DEFAULT`）。
    pub fn match_ip(&self, ip: &str) -> Option<&str> {
        let addr = parse_addr(ip)?;
        let mut best: Option<&crate::modules::RuleListEntry> = None;
        for entry in &self.entries {
            if !entry.net.contains(&addr) {
                continue;
            }
            match best {
                Some(current) if current.net.prefix_len() >= entry.net.prefix_len() => {}
                _ => best = Some(entry),
            }
        }
        best.map(|e| e.comment.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 对齐上游 `btn/BtnRulesetParsed`：把规则集 DTO 编译成可判定的形式。
#[derive(Debug, Clone, Default)]
pub struct BtnRulesetParsed {
    pub version: String,
    pub peer_id_rules: BTreeMap<String, RuleSet>,
    pub client_name_rules: BTreeMap<String, RuleSet>,
    pub ip_rules: BTreeMap<String, BtnIpList>,
    /// 端口规则：上游为每端口一个「内容等于端口号」的等值匹配器，这里保留端口号本身
    pub port_rules: BTreeMap<String, Vec<i32>>,
    pub script_rules: BTreeMap<String, String>,
}

impl BtnRulesetParsed {
    /// 对齐上游 `new BtnRulesetParsed(scriptEngineManager, btnRuleset, scriptExecute)`。
    ///
    /// 与上游一致：任一类别的规则文本非法（上游 `RuleParser.parse` 抛异常）时整体失败，
    /// 调用方应保留旧规则集（[`BtnNetworkOnline::apply_ruleset`] 即如此处理）。
    pub fn parse(ruleset: &BtnRuleset) -> anyhow::Result<Self> {
        let mut peer_id_rules = BTreeMap::new();
        for (category, raw) in &ruleset.peer_id_rules {
            peer_id_rules.insert(category.clone(), RuleSet::from_json_text(raw)?);
        }
        let mut client_name_rules = BTreeMap::new();
        for (category, raw) in &ruleset.client_name_rules {
            client_name_rules.insert(category.clone(), RuleSet::from_json_text(raw)?);
        }
        let version = ruleset.version_or_initial();
        let mut ip_rules = BTreeMap::new();
        for (category, raw) in &ruleset.ip_rules {
            ip_rules.insert(
                category.clone(),
                BtnIpList::from_cidrs(category, &version, raw),
            );
        }
        Ok(Self {
            version,
            peer_id_rules,
            client_name_rules,
            ip_rules,
            port_rules: ruleset.port_rules.clone(),
            script_rules: ruleset.script_rules.clone(),
        })
    }

    /// 对齐上游 `size()`：五类规则的条目总数（供状态展示/日志）。
    pub fn size(&self) -> usize {
        self.peer_id_rule_count()
            + self.client_name_rule_count()
            + self.ip_rule_count()
            + self.port_rule_count()
            + self.script_rule_count()
    }

    pub fn peer_id_rule_count(&self) -> usize {
        self.peer_id_rules.values().map(|r| r.rules.len()).sum()
    }

    pub fn client_name_rule_count(&self) -> usize {
        self.client_name_rules.values().map(|r| r.rules.len()).sum()
    }

    pub fn ip_rule_count(&self) -> usize {
        self.ip_rules.values().map(BtnIpList::len).sum()
    }

    pub fn port_rule_count(&self) -> usize {
        self.port_rules.values().map(Vec::len).sum()
    }

    pub fn script_rule_count(&self) -> usize {
        self.script_rules.len()
    }
}

/// BTN 的两组 IP 列表能力（对齐 `BtnAbilityIPAllowList` / `BtnAbilityIPDenyList`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnIpAbility {
    AllowList,
    DenyList,
}

impl BtnIpAbility {
    /// 上游缓存键前缀 `btn.ability.ip_allowlist.*` / `btn.ability.ip_denylist.*`
    pub fn cache_key(&self) -> &'static str {
        match self {
            BtnIpAbility::AllowList => "ip_allowlist",
            BtnIpAbility::DenyList => "ip_denylist",
        }
    }
}

/// BTN 规则拉取的**最小**传输抽象。
///
/// 上游由 `BtnNetwork`（okhttp + abilities 调度 + PoW captcha + `X-BTN-ContentVersion`）承担，
/// 本移植**不提供任何实现、也不发起网络请求**；无 BTN 服务器的部署不应触碰它
/// （模块保持未初始化 -> 恒 `pass()`），有 BTN 服务器的部署由应用层实现本 trait
/// 或直接调用 [`BtnNetworkOnline::apply_ruleset_json`] 等注入入口。
pub trait BtnTransport: Send + Sync {
    /// 拉取规则集；`Ok(None)` 表示服务端返回 204（无变化）。
    fn fetch_ruleset(&self, rev: &str) -> anyhow::Result<Option<BtnRuleset>>;
    /// 拉取 IP 列表；`Ok(None)` 表示 204（无变化）；返回 `(规则文本, X-BTN-ContentVersion)`。
    fn fetch_ip_list(
        &self,
        kind: BtnIpAbility,
        rev: &str,
    ) -> anyhow::Result<Option<(String, String)>>;
}

/// 对齐上游 `BtnNetworkOnline` 持有的 `btnNetwork.abilities`（仅保留判定所需部分）。
#[derive(Debug, Default)]
struct BtnState {
    /// 对齐上游 `btnNetwork != null`
    manager_initialized: bool,
    /// `BtnAbilityRules` 的规则集
    ruleset: Option<BtnRulesetParsed>,
    /// `BtnAbilityIPAllowList`
    ip_allow_list: Option<BtnIpList>,
    /// `BtnAbilityIPDenyList`
    ip_deny_list: Option<BtnIpList>,
}

/// 上游 `ScriptEngineManager.compileScript` 的产物（`CompiledScript`）。
struct BtnScript {
    name: String,
    ast: rhai::AST,
}

/// BTN 脚本规则的 rhai 引擎（构造方式与 [`crate::modules::expression_engine`] 完全一致）。
struct BtnScriptEngine {
    engine: Engine,
    /// 超时判定用的起始时间戳（原子；并发多 peer 执行时可能被交错覆盖，仅作安全上界）
    start: Arc<AtomicI64>,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 构造 rhai 引擎：注册 `peer` / `torrent` / `downloader` 等自定义类型与超时回调。
///
/// 类型注册与属性暴露统一在 [`crate::avscript::build_script_env`]（与
/// [`crate::modules::expression_engine`] 完全同一套，含上游驼峰 getter 与
/// `peer.peerAddress.{ip,port,address}`）。
fn build_script_engine() -> BtnScriptEngine {
    let env = crate::avscript::build_script_env();
    BtnScriptEngine {
        engine: env.engine,
        start: env.start,
    }
}

impl BtnScriptEngine {
    /// 对齐上游 `BtnRulesetParsed.compileScripts`：逐条编译，失败的记录日志并跳过。
    ///
    /// 上游脚本为 AviatorScript，先经 [`crate::avscript::transpile`] 翻译为 rhai。
    fn compile(&self, scripts: &BTreeMap<String, String>) -> Vec<BtnScript> {
        let mut compiled = Vec::new();
        tracing::info!("BTN 脚本规则编译开始，共 {} 条", scripts.len());
        for (name, content) in scripts {
            let translated = match crate::avscript::transpile(content) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(
                        "Unable to load BTN script {name}: AviatorScript 含暂不支持的语法（{e}）"
                    );
                    continue;
                }
            };
            // 展示名对齐上游 `AVScriptEngine.compileScript`：解析内容里的 `## @NAME`，
            // 缺省回退规则集 key
            let (meta_name, _) = crate::avscript::parse_metadata(content);
            let display = if meta_name.is_empty() {
                name.clone()
            } else {
                meta_name
            };
            match self.engine.compile(&translated) {
                Ok(ast) => compiled.push(BtnScript { name: display, ast }),
                Err(e) => tracing::error!("Unable to load BTN script {name}: {e}"),
            }
        }
        tracing::info!("BTN 脚本规则编译完成，成功 {} 条", compiled.len());
        compiled
    }

    /// 对齐上游 `BtnNetworkOnline.runExpression` + `ScriptEngineManager.handleResult`：
    /// 变量注入与返回值语义与 [`crate::modules::expression_engine`] 一致。
    ///
    /// 返回 `None` 表示 `pass()`（无动作）；脚本异常/超时同样按 `pass()` 处理。
    fn run(
        &self,
        script: &BtnScript,
        ban_duration_ms: i64,
        torrent: &TorrentData,
        peer: &PeerData,
        downloader_id: &str,
    ) -> Option<CheckResult> {
        let mut scope = Scope::new();
        scope.push_constant("peer", peer.clone());
        scope.push_constant("torrent", torrent.clone());
        scope.push_constant(
            "downloader",
            ScriptDownloader {
                id: downloader_id.to_string(),
                name: downloader_id.to_string(),
            },
        );
        scope.push_constant("banDuration", ban_duration_ms);
        scope.push_constant("cacheable", true);
        // 上游 BtnNetworkOnline 注入的是 `kvStorage`（非 expression-engine 的 ramStorage）
        scope.push_constant("kvStorage", rhai::Dynamic::from(rhai::Map::new()));
        scope.push_constant("moduleInstance", "btn".to_string());
        scope.push_constant("server", true);

        self.start.store(now_millis(), Ordering::Relaxed);
        let result = self.engine.eval_ast_with_scope(&mut scope, &script.ast);
        self.start.store(0, Ordering::Relaxed);

        match result {
            Ok(ret) => {
                let (action, payload) = handle_script_return(&ret)?;
                Some(build_script_result(
                    action,
                    ban_duration_ms,
                    &script.name,
                    &payload,
                ))
            }
            Err(e) => {
                tracing::debug!("BTN 脚本 {} 执行异常，按 pass 处理: {e}", script.name);
                None
            }
        }
    }
}

/// 上游 `ScriptEngineManager.handleResult` 的返回值映射（与
/// [`crate::modules::expression_engine`] 的 `handle_return` 逐项一致）。
fn handle_script_return(ret: &rhai::Dynamic) -> Option<(PeerAction, String)> {
    if ret.is_bool() {
        return if ret.as_bool().unwrap_or(false) {
            Some((PeerAction::Ban, "true".to_string()))
        } else {
            None
        };
    }
    if ret.is_int() {
        return match ret.as_int().unwrap_or(0) {
            0 => None,
            1 => Some((PeerAction::Ban, "1".to_string())),
            2 => Some((PeerAction::Skip, "2".to_string())),
            _ => None,
        };
    }
    if ret.is_float() {
        // 上游 `number.intValue()` 为向零截断
        let v = ret.as_float().unwrap_or(0.0) as i64;
        return match v {
            0 => None,
            1 => Some((PeerAction::Ban, v.to_string())),
            2 => Some((PeerAction::Skip, v.to_string())),
            _ => None,
        };
    }
    if let Ok(s) = ret.clone().into_string() {
        if s.trim().is_empty() {
            return None;
        }
        return if let Some(rest) = s.strip_prefix('@') {
            Some((PeerAction::Skip, rest.to_string()))
        } else {
            Some((PeerAction::Ban, s))
        };
    }
    // 其它类型（含上游的 PeerAction / CheckResult 返回）→ 视作无效，pass
    None
}

/// 与 [`crate::modules::expression_engine`] 的 `build_result` 一致，只是模块名为 `btn`。
fn build_script_result(
    action: PeerAction,
    ban_duration_ms: i64,
    name: &str,
    payload: &str,
) -> CheckResult {
    match action {
        PeerAction::Skip => CheckResult {
            module: "btn".to_string(),
            action: PeerAction::Skip,
            ban_duration_ms: 0,
            rule: "btn".to_string(),
            reason: payload.to_string(),
            data: serde_json::json!({ "script": name }),
            rule_key: Some(TranslationComponent::new("USER_SCRIPT_RULE")),
            reason_key: Some(TranslationComponent::new(payload)),
        },
        PeerAction::Ban => CheckResult::ban(
            "btn",
            ban_duration_ms,
            "btn",
            &format!("Script {name}: {payload}"),
            serde_json::json!({ "script": name }),
        )
        .with_keys(
            TranslationComponent::new("USER_SCRIPT_RULE"),
            TranslationComponent::with_params(
                "USER_SCRIPT_RUN_RESULT",
                vec![name.to_string().into(), payload.to_string().into()],
            ),
        ),
        _ => CheckResult::pass("btn"),
    }
}

/// BTN 网络在线规则模块（配置键 `btn`）。
pub struct BtnNetworkOnline {
    /// `module.btn.ban-duration`
    pub ban_duration_ms: i64,
    /// `btn.allow-script-execute`（上游主配置 `btn.allow-script-execute`，默认 false）
    allow_script: AtomicBool,
    script_engine: BtnScriptEngine,
    /// 编译后的 BTN 脚本规则（仅 `allow_script` 为真时非空）
    scripts: RwLock<Vec<BtnScript>>,
    state: RwLock<BtnState>,
}

impl BtnNetworkOnline {
    /// `ban_duration_ms` 来自 `profile.yml` 的 `module.btn.ban-duration`（上游默认 [`BTN_BAN_DURATION_MS`]）。
    pub fn new(ban_duration_ms: i64) -> Self {
        Self {
            ban_duration_ms,
            allow_script: AtomicBool::new(false),
            script_engine: build_script_engine(),
            scripts: RwLock::new(Vec::new()),
            state: RwLock::new(BtnState::default()),
        }
    }

    /// 设置 `module.btn.allow-script-execute`。
    pub fn with_allow_script(self, allow_script: bool) -> Self {
        self.set_allow_script(allow_script);
        self
    }

    /// 上游 `BtnNetworkOnline.reloadConfig()` 的 `allow-script-execute`；传输层握手时同步。
    ///
    /// 关闭时清空已编译脚本（等价于上游 `scriptRules` 为空）；打开时按当前规则集重新编译。
    pub fn set_allow_script(&self, allow_script: bool) {
        self.allow_script.store(allow_script, Ordering::Relaxed);
        let pending = if allow_script {
            self.state
                .read()
                .ok()
                .and_then(|state| state.ruleset.as_ref().map(|r| r.script_rules.clone()))
        } else {
            None
        };
        match pending {
            Some(rules) => self.compile_scripts(&rules),
            None => {
                if let Ok(mut scripts) = self.scripts.write() {
                    scripts.clear();
                }
            }
        }
    }

    pub fn allow_script(&self) -> bool {
        self.allow_script.load(Ordering::Relaxed)
    }

    fn compile_scripts(&self, rules: &BTreeMap<String, String>) {
        let compiled = self.script_engine.compile(rules);
        if let Ok(mut scripts) = self.scripts.write() {
            *scripts = compiled;
        }
    }

    /// 标记 BTN 客户端已初始化（对齐上游 `btnNetwork != null`）。
    ///
    /// 应用层仅在主配置 `btn.enabled: true` 且成功握手拿到 abilities 后调用；
    /// 未标记时本模块恒返回 `pass()`，绝不封禁。
    pub fn set_manager_initialized(&self, initialized: bool) {
        if let Ok(mut state) = self.state.write() {
            state.manager_initialized = initialized;
        }
    }

    pub fn is_manager_initialized(&self) -> bool {
        self.state
            .read()
            .map(|s| s.manager_initialized)
            .unwrap_or(false)
    }

    /// 注入 BTN 规则集（对齐上游 `BtnAbilityRules.updateRule` 的成功分支：
    /// `new BtnRulesetParsed(...)` 替换 `btnRule`，并 `invalidateAll()` 判定缓存）。
    ///
    /// 解析失败时**保留旧规则集**（对齐上游异常被外层 `catch` 吞掉、`btnRule` 不变）。
    /// 注入成功即视为 BTN 客户端已就绪。
    pub fn apply_ruleset(&self, ruleset: &BtnRuleset) -> anyhow::Result<()> {
        let parsed = BtnRulesetParsed::parse(ruleset)?;
        let script_rules = parsed.script_rules.clone();
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("BTN 模块状态锁已中毒"))?;
        state.ruleset = Some(parsed);
        state.manager_initialized = true;
        drop(state);
        // 上游 `new BtnRulesetParsed(scriptEngineManager, btnRuleset, scriptExecute)`：
        // 同一处按 `scriptExecute` 决定是否编译脚本规则
        if self.allow_script() {
            self.compile_scripts(&script_rules);
        } else if let Ok(mut scripts) = self.scripts.write() {
            scripts.clear();
        }
        Ok(())
    }

    /// 注入规则集 JSON（对齐上游 `JsonUtil.getGson().fromJson(responseBody, BtnRuleset.class)`）。
    pub fn apply_ruleset_json(&self, json: &str) -> anyhow::Result<()> {
        let ruleset: BtnRuleset =
            serde_json::from_str(json).map_err(|e| anyhow::anyhow!("BTN 规则集 JSON 非法: {e}"))?;
        self.apply_ruleset(&ruleset)
    }

    /// 注入 IP 白名单文本（对齐 `BtnAbilityIPAllowList.updateRule` 的
    /// `ipMatcher.setData("BTN AllowList (Remote)", ...)`）；返回解析到的规则条数。
    pub fn apply_ip_allowlist_text(&self, text: &str, version: &str) -> usize {
        let (list, loaded) = BtnIpList::from_rule_text("BTN AllowList (Remote)", version, text);
        if let Ok(mut state) = self.state.write() {
            state.ip_allow_list = Some(list);
            state.manager_initialized = true;
        }
        loaded
    }

    /// 注入 IP 黑名单文本（对齐 `BtnAbilityIPDenyList.updateRule` 的
    /// `ipMatcher.setData("BTN DenyList (Remote)", ...)`）；返回解析到的规则条数。
    pub fn apply_ip_denylist_text(&self, text: &str, version: &str) -> usize {
        let (list, loaded) = BtnIpList::from_rule_text("BTN DenyList (Remote)", version, text);
        if let Ok(mut state) = self.state.write() {
            state.ip_deny_list = Some(list);
            state.manager_initialized = true;
        }
        loaded
    }

    /// 一次「拉取 → 注入」同步（上游 `BtnAbilityRules.updateRule` /
    /// `BtnAbilityIP*List.updateRule` 的定时任务体）。
    ///
    /// 只有配置了 BTN 服务器的应用层才应调用；**任何**拉取/解析失败都保留当前规则集
    /// （对齐上游 catch 后 `setLastStatus(false)` 的优雅降级），模块绝不会因为失败而封禁，
    /// 也绝不会因失败而清空既有规则。返回 `true` 表示规则集被更新（用于日志/状态展示）。
    pub fn sync_from_transport(&self, transport: &dyn BtnTransport) -> bool {
        let mut updated = false;
        let ruleset_rev = self
            .ruleset_version()
            .unwrap_or_else(|| "initial".to_string());
        match transport.fetch_ruleset(&ruleset_rev) {
            Ok(Some(ruleset)) => match self.apply_ruleset(&ruleset) {
                Ok(()) => updated = true,
                // 上游：JsonSyntaxException / RuleParser 异常 -> 保留旧 btnRule
                Err(e) => tracing::warn!("BTN 规则集解析失败，保留旧规则: {e}"),
            },
            // 上游：HTTP 204 -> 规则未变化
            Ok(None) => {}
            Err(e) => tracing::warn!("BTN 规则集拉取失败，保留旧规则: {e}"),
        }

        for (kind, allow) in [
            (BtnIpAbility::AllowList, true),
            (BtnIpAbility::DenyList, false),
        ] {
            let rev = self.ip_list_version(allow);
            match transport.fetch_ip_list(kind, &rev) {
                Ok(Some((text, version))) => {
                    let loaded = if allow {
                        self.apply_ip_allowlist_text(&text, &version)
                    } else {
                        self.apply_ip_denylist_text(&text, &version)
                    };
                    tracing::debug!("BTN {} 列表已更新，共 {loaded} 条", kind.cache_key());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("BTN {} 列表拉取失败，保留旧列表: {e}", kind.cache_key()),
            }
        }
        updated
    }

    /// 当前允许（`allow=true`）/拒绝列表的 `X-BTN-ContentVersion`；未加载时为 `"initial"`。
    pub fn ip_list_version(&self, allow: bool) -> String {
        self.state
            .read()
            .ok()
            .and_then(|s| {
                let list = if allow {
                    s.ip_allow_list.as_ref()
                } else {
                    s.ip_deny_list.as_ref()
                };
                list.map(|l| l.version.clone())
            })
            .unwrap_or_else(|| "initial".to_string())
    }

    /// 清空全部已加载规则（对齐上游 `BtnNetwork.resetAbilities()` / `BtnAbility.unload()`）。
    pub fn unload(&self) {
        if let Ok(mut state) = self.state.write() {
            *state = BtnState::default();
        }
        if let Ok(mut scripts) = self.scripts.write() {
            scripts.clear();
        }
    }

    pub fn ruleset_version(&self) -> Option<String> {
        self.state
            .read()
            .ok()
            .and_then(|s| s.ruleset.as_ref().map(|r| r.version.clone()))
    }

    /// 对齐上游 `BtnNetworkOnline.checkScript`：逐条执行已编译的 BTN 脚本规则。
    ///
    /// 聚合语义与上游一致：SKIP 优先级最高（命中即短路返回），其次 BAN（保留最后一个），
    /// 全部 pass 则返回 `None`（调用方继续 `checkShouldBanModern` / `checkShouldBanLegacy`）。
    ///
    /// 上游把脚本提交到 `parallelService` 并行执行；本移植按脚本名顺序（BTreeMap）串行执行，
    /// 结果集合相同（SKIP 短路在并行版本下同样存在竞态，这里取确定性顺序）。
    fn check_script(
        &self,
        torrent: &TorrentData,
        peer: &PeerData,
        downloader_id: &str,
    ) -> Option<CheckResult> {
        let Ok(scripts) = self.scripts.read() else {
            return None;
        };
        let mut decided: Option<CheckResult> = None;
        for script in scripts.iter() {
            let Some(result) =
                self.script_engine
                    .run(script, self.ban_duration_ms, torrent, peer, downloader_id)
            else {
                continue;
            };
            match result.action {
                // 上游 `if (result.action() == PeerAction.SKIP) return result;`
                PeerAction::Skip => return Some(result),
                // 上游 `else if (result.action() == PeerAction.BAN) finalResult = result;`
                PeerAction::Ban => decided = Some(result),
                _ => {}
            }
        }
        decided
    }

    /// 对齐上游 `@Subscribe onRuleUpdate(BtnRuleUpdateEvent)`：
    /// IP 白名单更新后，把**已在封禁表中**且命中白名单的地址解封。
    ///
    /// 上游由事件总线在允许列表拉取成功后触发；本移植无事件总线，由上层在注入白名单后显式调用。
    pub fn on_rule_update(&self, ban_list: &Arc<StdMutex<BanList>>) {
        let Ok(state) = self.state.read() else {
            return;
        };
        // 上游：allowlist ability 不存在时直接返回
        let Some(allow_list) = &state.ip_allow_list else {
            return;
        };
        let banned: Vec<String> = match ban_list.lock() {
            Ok(list) => list.iter().map(|(ip, _)| ip.clone()).collect(),
            Err(_) => return,
        };
        let pending_unban: Vec<String> = banned
            .into_iter()
            .filter(|ip| allow_list.match_ip(ip).is_some())
            .collect();
        if pending_unban.is_empty() {
            return;
        }
        if let Ok(mut list) = ban_list.lock() {
            for ip in pending_unban {
                list.remove(&ip);
            }
        }
    }
}

impl RuleModule for BtnNetworkOnline {
    fn name(&self) -> &str {
        "BTN Network Online Rules"
    }

    fn config_name(&self) -> &str {
        "btn"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peer: &PeerData,
        _ctx: &CheckContext,
    ) -> CheckResult {
        let module = self.config_name().to_string();
        let Ok(state) = self.state.read() else {
            return CheckResult::pass(&module);
        };
        if !state.manager_initialized {
            // 上游：`btnNetwork == null` -> BTN_MANAGER_NOT_INITIALIZED（NO_ACTION，永不封禁）。
            // 本移植按要求统一返回 pass()，见文件头「唯一有意差异」。
            return CheckResult::pass(&module);
        }

        // checkShouldSkip：IP 白名单命中 -> SKIP（ban-duration 0）
        if let Some(allow_list) = &state.ip_allow_list {
            if let Some(comment) = allow_list.match_ip(&peer.ip) {
                return allowlist_skip_result(&module, comment);
            }
        }

        // checkScript：脚本规则（`btn.allow-script-execute`，rhai 引擎）
        //
        // 上游：`rule == null` ⇒ pass；`isHandShaking(peer)` ⇒ handshaking()（同样是
        // NO_ACTION，不会短路后续的 `checkShouldBanModern`），否则逐条执行脚本：
        // SKIP 立即返回、BAN 覆盖保留，最终返回最后一个 BAN。
        if self.allow_script() && state.ruleset.is_some() && !peer.is_handshaking() {
            if let Some(result) = self.check_script(torrent, peer, downloader_id) {
                return result;
            }
        }

        // checkShouldBanModern：IP 黑名单命中 -> BAN（此路径没有握手闸门）
        if let Some(deny_list) = &state.ip_deny_list {
            if let Some(comment) = deny_list.match_ip(&peer.ip) {
                return denylist_ban_result(&module, self.ban_duration_ms, comment);
            }
        }

        // checkShouldBanLegacy
        let Some(ruleset) = &state.ruleset else {
            // 上游：`ruleAbility.getBtnRule() == null` -> pass()（此时也不做握手闸门）
            return CheckResult::pass(&module);
        };
        if peer.is_handshaking() {
            // 上游：`isHandShaking(peer)` -> handshaking()
            return CheckResult::handshaking(&module);
        }

        // 上游顺序：peerId -> clientName -> ip -> port（仅当对应类别非空才参与）
        let mut candidates: Vec<Option<CheckResult>> = Vec::with_capacity(4);
        if !ruleset.peer_id_rules.is_empty() {
            candidates.push(check_peer_id_rule(
                &module,
                self.ban_duration_ms,
                ruleset,
                peer,
            ));
        }
        if !ruleset.client_name_rules.is_empty() {
            candidates.push(check_client_name_rule(
                &module,
                self.ban_duration_ms,
                ruleset,
                peer,
            ));
        }
        if !ruleset.ip_rules.is_empty() {
            candidates.push(check_ip_rule(&module, self.ban_duration_ms, ruleset, peer));
        }
        if !ruleset.port_rules.is_empty() {
            candidates.push(check_port_rule(
                &module,
                self.ban_duration_ms,
                ruleset,
                peer,
            ));
        }

        let mut last_ban: Option<CheckResult> = None;
        for result in candidates.into_iter().flatten() {
            if result.action == PeerAction::Skip {
                // 上游 Early exit：任一 SKIP 立即返回
                return result;
            }
            if result.action == PeerAction::Ban {
                // 上游：命中 BAN 时覆盖，最终上报最后一个 BAN
                last_ban = Some(result);
            }
        }
        last_ban.unwrap_or_else(|| CheckResult::pass(&module))
    }
}

/// 上游 `BTN_MANAGER_NOT_INITIALIZED` 的原始形态（`NO_ACTION` + `GENERAL_NA` +
/// `status: btn_manager_not_initialized`）。
///
/// 本模块的 `check` 按任务要求改为返回 `pass()`（见文件头），此函数保留上游结果以便上层
/// 展示/对照，两者都是 `NO_ACTION`、都绝不封禁。
pub fn btn_manager_not_initialized_result(module: &str) -> CheckResult {
    CheckResult {
        module: module.to_string(),
        action: PeerAction::NoAction,
        ban_duration_ms: 0,
        rule: "N/A".to_string(),
        reason: "BtnManager not initialized".to_string(),
        data: serde_json::json!({ "status": "btn_manager_not_initialized" }),
        rule_key: Some(TranslationComponent::new("GENERAL_NA")),
        reason_key: Some(TranslationComponent::new("BtnManager not initialized")),
    }
}

/// 对齐上游 `checkShouldSkip` 命中（`BtnAbilityIPAllowList`）。
fn allowlist_skip_result(module: &str, comment: &str) -> CheckResult {
    CheckResult {
        module: module.to_string(),
        action: PeerAction::Skip,
        ban_duration_ms: 0,
        rule: "IP Allowlist".to_string(),
        reason: format!("Matched allowlist: {comment}"),
        data: serde_json::json!({ "type": "ipAllowList", "matchedValue": comment }),
        rule_key: Some(TranslationComponent::new("BTN_ABILITY_IP_ALLOWLIST_RULE")),
        reason_key: Some(TranslationComponent::with_params(
            "BTN_ABILITY_IP_ALLOWLIST_HIT",
            vec![comment.to_string().into()],
        )),
    }
}

/// 对齐上游 `checkShouldBanModern` 命中（`BtnAbilityIPDenyList`）。
fn denylist_ban_result(module: &str, ban_duration_ms: i64, comment: &str) -> CheckResult {
    CheckResult {
        module: module.to_string(),
        action: PeerAction::Ban,
        ban_duration_ms,
        rule: "IP Denylist".to_string(),
        reason: format!("Matched denylist: {comment}"),
        data: serde_json::json!({ "type": "ip" }),
        rule_key: Some(TranslationComponent::new("BTN_ABILITY_IP_DENYLIST_RULE")),
        reason_key: Some(TranslationComponent::with_params(
            "BTN_ABILITY_IP_DENYLIST_HIT",
            vec![comment.to_string().into()],
        )),
    }
}

/// 对齐上游 `checkPeerIdRule` / `checkClientNameRule` / `checkIpRule` / `checkPortRule`
/// 共用的 `CheckResult` 构造：
/// ```text
/// rule   = BTN_BTN_RULE(category, matcherName)          // IP 类别下第二个参数为 category 本身
/// reason = MODULE_BTN_BAN(peerType, category, 第三参数)  // 第三参数为 matcherName 或 peer 地址
/// data   = { type, category, rule? }
/// ```
///
/// `matcher_literal` 仅用于 `rule`/`reason` 的字面量槽位（上游这两处是 `TranslationComponent`，
/// 权威文案在 `rule_key` / `reason_key` 中）。
///
/// `matcher_name` / `reason_matcher` 用 [`Param`] 而非 `TranslationComponent`：上游在
/// IP 类别下传的是**普通字符串**（`category` / `pa.toString()`），在其它类别下传的是
/// `matcherName()` 这个 `TranslationComponent`，两者在参数渲染上等价但结构不同。
#[allow(clippy::too_many_arguments)]
fn btn_rule_ban(
    module: &str,
    ban_duration_ms: i64,
    peer_type: &str,
    category: &str,
    matcher_name: Param,
    reason_matcher: Param,
    matcher_literal: &str,
    data: serde_json::Value,
) -> CheckResult {
    CheckResult {
        module: module.to_string(),
        action: PeerAction::Ban,
        ban_duration_ms,
        rule: format!("BTN-{category}-{matcher_literal}"),
        reason: format!("[BTN Ban] Match {peer_type} ruleset ({category}): {matcher_literal}"),
        data,
        rule_key: Some(TranslationComponent::with_params(
            "BTN_BTN_RULE",
            vec![Param::Text(category.to_string()), matcher_name],
        )),
        reason_key: Some(TranslationComponent::with_params(
            "MODULE_BTN_BAN",
            vec![
                Param::Text(peer_type.to_string()),
                Param::Text(category.to_string()),
                reason_matcher,
            ],
        )),
    }
}

/// 取命中规则（对齐上游 `matchResult.rule()`；未命中时下标为 -1）。
fn matched_matcher(rules: &RuleSet, index: isize) -> Option<&Matcher> {
    if index < 0 {
        return None;
    }
    rules.rules.get(index as usize)
}

/// 对齐上游 `checkPeerIdRule`：按类别顺序对 `peer.getPeerId()` 应用 `RuleParser.matchRule`
/// （`RuleSet::match`：FALSE 短路，TRUE 可被后续覆盖）。
fn check_peer_id_rule(
    module: &str,
    ban_duration_ms: i64,
    ruleset: &BtnRulesetParsed,
    peer: &PeerData,
) -> Option<CheckResult> {
    for (category, rules) in &ruleset.peer_id_rules {
        let result = rules.r#match(peer.peer_id.as_deref());
        if result.hit {
            let matcher = matched_matcher(rules, result.index)?;
            let metadata = matcher.metadata();
            let mut data = serde_json::json!({ "type": "peerId", "category": category });
            data["rule"] = serde_json::Value::String(metadata.clone());
            return Some(btn_rule_ban(
                module,
                ban_duration_ms,
                "PeerId",
                category,
                Param::from(matcher.name_component()),
                Param::from(matcher.name_component()),
                &metadata,
                data,
            ));
        }
    }
    None
}

/// 对齐上游 `checkClientNameRule`：按类别顺序对 `peer.getClientName()` 应用 `RuleParser.matchRule`。
fn check_client_name_rule(
    module: &str,
    ban_duration_ms: i64,
    ruleset: &BtnRulesetParsed,
    peer: &PeerData,
) -> Option<CheckResult> {
    for (category, rules) in &ruleset.client_name_rules {
        let result = rules.r#match(peer.client_name.as_deref());
        if result.hit {
            let matcher = matched_matcher(rules, result.index)?;
            let metadata = matcher.metadata();
            let mut data = serde_json::json!({ "type": "clientName", "category": category });
            data["rule"] = serde_json::Value::String(metadata.clone());
            return Some(btn_rule_ban(
                module,
                ban_duration_ms,
                "ClientName",
                category,
                Param::from(matcher.name_component()),
                Param::from(matcher.name_component()),
                &metadata,
                data,
            ));
        }
    }
    None
}

/// 对齐上游 `checkIpRule`：peer 地址先按 `isIPv4Convertible -> toIPv4` 归一化
/// （[`parse_addr`] 已把 IPv4-mapped IPv6 视为 IPv4），再按类别顺序做 IP 匹配；
/// 地址不可解析时该类别不产生结果（上游 `pa == null` 直接 `return null`）。
fn check_ip_rule(
    module: &str,
    ban_duration_ms: i64,
    ruleset: &BtnRulesetParsed,
    peer: &PeerData,
) -> Option<CheckResult> {
    let pa = parse_addr(&peer.ip)?.to_string();
    for (category, matcher) in &ruleset.ip_rules {
        if matcher.match_ip(&pa).is_some() {
            // 上游：rule = BTN_BTN_RULE(category, category)、reason = MODULE_BTN_BAN("IP", category, pa)
            // 注意两个参数都是**普通字符串**，不是 TranslationComponent
            return Some(btn_rule_ban(
                module,
                ban_duration_ms,
                "IP",
                category,
                Param::Text(category.clone()),
                Param::Text(pa.clone()),
                &pa,
                serde_json::json!({ "type": "ip", "category": category }),
            ));
        }
    }
    None
}

/// 对齐上游 `checkPortRule`：`RuleParser.matchRule(portRules.get(category), Integer.toString(port))`。
///
/// 端口规则由 `BtnRulesetParsed.parsePortRule` 生成为「内容等于端口号」的等值匹配器
/// （`Integer.parseInt(content) == s`，命中 TRUE、否则 DEFAULT、永不 FALSE），因此这里
/// 逐条比较端口号并保留**最后一个**命中项，与 `matchRule` 的覆盖语义一致。
/// matcherName 保持上游的 `Lang.BTN_PORT_RULE(version)`。
fn check_port_rule(
    module: &str,
    ban_duration_ms: i64,
    ruleset: &BtnRulesetParsed,
    peer: &PeerData,
) -> Option<CheckResult> {
    for (category, ports) in &ruleset.port_rules {
        let mut matched: Option<i32> = None;
        for port in ports {
            if i64::from(*port) == i64::from(peer.port) {
                matched = Some(*port);
            }
        }
        if let Some(port) = matched {
            let matcher_name = TranslationComponent::with_params(
                "BTN_PORT_RULE",
                vec![ruleset.version.clone().into()],
            );
            let literal = port.to_string();
            let mut data = serde_json::json!({ "type": "port", "category": category });
            data["rule"] = serde_json::Value::String(literal.clone());
            return Some(btn_rule_ban(
                module,
                ban_duration_ms,
                "Port",
                category,
                Param::from(matcher_name.clone()),
                Param::from(matcher_name),
                &literal,
                data,
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Param;

    fn torrent() -> TorrentData {
        TorrentData {
            hash: "h".to_string(),
            name: "t".to_string(),
            progress: 1.0,
            total_size: 1_000_000_000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        }
    }

    fn peer(ip: &str, port: u16, peer_id: &str, client: &str) -> PeerData {
        PeerData {
            client_name: Some(client.to_string()),
            peer_id: Some(peer_id.to_string()),
            dl_speed: 1000,
            downloaded: 1000,
            up_speed: 1000,
            uploaded: 1000,
            progress: 0.5,
            flags: Some("d u".to_string()),
            ip: ip.to_string(),
            port,
            raw_ip: format!("{ip}:{port}"),
            connection: Some("uTP".to_string()),
        }
    }

    /// 握手中的 peer：上下行速率皆为 0（`Peer.isHandshaking()`）
    fn handshaking_peer(ip: &str, port: u16, peer_id: &str, client: &str) -> PeerData {
        let mut p = peer(ip, port, peer_id, client);
        p.up_speed = 0;
        p.dl_speed = 0;
        p
    }

    fn module_with_ruleset(json: &str) -> BtnNetworkOnline {
        let module = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        module.apply_ruleset_json(json).expect("规则集必须可解析");
        module
    }

    fn check(module: &BtnNetworkOnline, peer: &PeerData) -> CheckResult {
        module.check("qbittorrent", &torrent(), peer, &CheckContext::default())
    }

    fn matcher_name_starts_with(content: &str) -> TranslationComponent {
        TranslationComponent::with_params("MATCH_STRING_STARTS_WITH", vec![content.into()])
    }

    /// (a) 未加载任何规则（未初始化 / 已初始化但无规则集）⇒ pass，绝不封禁
    #[test]
    fn no_rules_loaded_passes_and_never_bans() {
        // 从未初始化（对齐上游 btnNetwork == null）
        let fresh = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        let result = check(
            &fresh,
            &peer("1.2.3.4", 51413, "-hp001-xxxxxxxxxxxx", "Xunlei"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.ban_duration_ms, 0);
        assert_eq!(result.data["status"], "pass");
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::new("Check passed"))
        );

        // 已初始化但规则集为空：上游所有 ability 为 null -> 逐步 pass()
        let initialized = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        initialized.set_manager_initialized(true);
        let result = check(
            &initialized,
            &peer("1.2.3.4", 51413, "-hp001-xxxxxxxxxxxx", "Xunlei"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.data["status"], "pass");

        // 空规则集（各类别都为空）同样不封禁
        let empty = module_with_ruleset(r#"{"version":"v1"}"#);
        let result = check(
            &empty,
            &peer("1.2.3.4", 51413, "-hp001-xxxxxxxxxxxx", "Xunlei"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.data["status"], "pass");
    }

    /// (b) 命中 peer-id 规则 ⇒ BAN，rule/reason 使用上游 BTN_BTN_RULE / MODULE_BTN_BAN
    #[test]
    fn peer_id_rule_ban_reports_upstream_keys() {
        let module = module_with_ruleset(
            r#"{
                "version": "v9.5.1",
                "peer_id": { "xunlei": ["{\"method\":\"STARTS_WITH\",\"content\":\"-hp\"}"] }
            }"#,
        );
        let result = check(
            &module,
            &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Ban);
        assert_eq!(result.module, "btn");
        assert_eq!(result.ban_duration_ms, BTN_BAN_DURATION_MS);
        assert_eq!(result.data["type"], "peerId");
        assert_eq!(result.data["category"], "xunlei");
        assert_eq!(result.data["rule"], "-hp");
        let name = matcher_name_starts_with("-hp");
        assert_eq!(
            result.rule_key,
            Some(TranslationComponent::with_params(
                "BTN_BTN_RULE",
                vec!["xunlei".into(), name.clone().into()],
            ))
        );
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::with_params(
                "MODULE_BTN_BAN",
                vec!["PeerId".into(), "xunlei".into(), name.into()],
            ))
        );
    }

    /// (c) 命中 client-name 规则 ⇒ BAN
    #[test]
    fn client_name_rule_ban_reports_upstream_keys() {
        let module = module_with_ruleset(
            r#"{
                "version": "v9.5.1",
                "client_name": { "xunlei": ["{\"method\":\"CONTAINS\",\"content\":\"xunlei\"}"] }
            }"#,
        );
        let result = check(
            &module,
            &peer("9.9.9.9", 51413, "-qb0000-abcdefghijkl", "Xunlei 1.0"),
        );
        assert_eq!(result.action, PeerAction::Ban);
        assert_eq!(result.ban_duration_ms, BTN_BAN_DURATION_MS);
        assert_eq!(result.data["type"], "clientName");
        assert_eq!(result.data["category"], "xunlei");
        let name =
            TranslationComponent::with_params("MATCH_STRING_CONTAINS", vec!["xunlei".into()]);
        assert_eq!(
            result.rule_key,
            Some(TranslationComponent::with_params(
                "BTN_BTN_RULE",
                vec!["xunlei".into(), name.clone().into()],
            ))
        );
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::with_params(
                "MODULE_BTN_BAN",
                vec!["ClientName".into(), "xunlei".into(), name.into()],
            ))
        );
    }

    /// (d) 命中规则集内的 IP 规则 ⇒ BAN（rule 的第二参数为 category 本身，reason 第三参数为 IP）
    #[test]
    fn ip_rule_ban_reports_upstream_keys() {
        let module = module_with_ruleset(
            r#"{
                "version": "v9.5.1",
                "ip": { "malicious": ["1.2.3.0/24", "2001:db8::/32"] }
            }"#,
        );
        // IPv4：规则集内 CIDR 命中
        let result = check(
            &module,
            &peer("1.2.3.77", 51413, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Ban);
        assert_eq!(result.ban_duration_ms, BTN_BAN_DURATION_MS);
        assert_eq!(result.data["type"], "ip");
        assert_eq!(result.data["category"], "malicious");
        assert_eq!(
            result.rule_key,
            Some(TranslationComponent::with_params(
                "BTN_BTN_RULE",
                vec!["malicious".into(), "malicious".into()],
            ))
        );
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::with_params(
                "MODULE_BTN_BAN",
                vec!["IP".into(), "malicious".into(), "1.2.3.77".into()],
            ))
        );

        // IPv6 规则同样命中
        let result = check(
            &module,
            &peer("2001:db8::1", 51413, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Ban);
        // IPv4-mapped IPv6 归一化为 IPv4 后再匹配
        let result = check(
            &module,
            &peer(
                "::ffff:1.2.3.77",
                51413,
                "-qb0000-abcdefghijkl",
                "qBittorrent",
            ),
        );
        assert_eq!(result.data["category"], "malicious");
    }

    /// 端口规则：matcherName 为 BTN_PORT_RULE(version)，data 带端口号
    #[test]
    fn port_rule_ban_uses_btn_port_rule_matcher() {
        let module = module_with_ruleset(r#"{"version":"v9.5.1","port":{"badport":[51413]}}"#);
        let result = check(
            &module,
            &peer("9.9.9.9", 51413, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Ban);
        assert_eq!(result.data["type"], "port");
        assert_eq!(result.data["category"], "badport");
        assert_eq!(result.data["rule"], "51413");
        let name =
            TranslationComponent::with_params("BTN_PORT_RULE", vec!["v9.5.1".to_string().into()]);
        assert_eq!(
            result.rule_key,
            Some(TranslationComponent::with_params(
                "BTN_BTN_RULE",
                vec!["badport".into(), name.clone().into()],
            ))
        );
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::with_params(
                "MODULE_BTN_BAN",
                vec!["Port".into(), "badport".into(), name.into()],
            ))
        );

        // 端口不同 ⇒ pass
        let result = check(
            &module,
            &peer("9.9.9.9", 12345, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
    }

    /// (e) 未命中任何规则 ⇒ pass
    #[test]
    fn non_matching_peer_passes() {
        let module = module_with_ruleset(
            r#"{
                "version": "v9.5.1",
                "peer_id": { "xunlei": ["{\"method\":\"STARTS_WITH\",\"content\":\"-hp\"}"] },
                "client_name": { "xunlei": ["{\"method\":\"CONTAINS\",\"content\":\"xunlei\"}"] },
                "ip": { "malicious": ["1.2.3.0/24"] },
                "port": { "badport": [51413] }
            }"#,
        );
        let result = check(
            &module,
            &peer("8.8.8.8", 12345, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.ban_duration_ms, 0);
        assert_eq!(result.data["status"], "pass");
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::new("Check passed"))
        );

        // 规则集的 FALSE 短路：显式 FALSE 规则使 peer-id 规则集整体不命中
        let module = module_with_ruleset(
            r#"{
                "peer_id": { "xunlei": ["{\"method\":\"EQUALS\",\"content\":\"qbittorrent\",\"hit\":\"FALSE\"}"] }
            }"#,
        );
        let result = check(
            &module,
            &peer("8.8.8.8", 12345, "qbittorrent", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
    }

    /// (f) 握手闸门与上游一致：仅在「规则集已加载」的导入路径上生效
    #[test]
    fn handshake_gate_matches_upstream() {
        let json = r#"{
            "version": "v9.5.1",
            "peer_id": { "xunlei": ["{\"method\":\"STARTS_WITH\",\"content\":\"-hp\"}"] }
        }"#;

        // 规则集已加载 + 握手中 ⇒ handshaking()（即使命中规则也不封禁）
        let module = module_with_ruleset(json);
        let result = check(
            &module,
            &handshaking_peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.ban_duration_ms, 0);
        assert_eq!(result.data["status"], "handshaking");
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::new("Peer handshaking"))
        );

        // 未加载规则集 + 握手中 ⇒ pass（上游 `rule == null` 在握手判断之前返回 pass()）
        let initialized = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        initialized.set_manager_initialized(true);
        let result = check(
            &initialized,
            &handshaking_peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.data["status"], "pass");

        // 握手完成后同一 peer 命中规则 ⇒ BAN
        let result = check(
            &module,
            &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Ban);
    }

    /// 现代协议的两个 IP 列表：白名单（SKIP）在黑名单（BAN）之前判定
    #[test]
    fn ip_allowlist_skip_precedes_denylist_ban() {
        let module = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        let loaded = module.apply_ip_denylist_text("2.2.2.0/24 # 恶意网段\n", "v1");
        assert_eq!(loaded, 1);
        assert_eq!(
            module.apply_ip_allowlist_text("2.2.2.2 # 白名单\n", "v1"),
            1
        );

        // 白名单命中 ⇒ SKIP（ban-duration 0，带 matchedValue）
        let result = check(
            &module,
            &peer("2.2.2.2", 51413, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Skip);
        assert_eq!(result.ban_duration_ms, 0);
        assert_eq!(result.data["type"], "ipAllowList");
        assert_eq!(result.data["matchedValue"], " 白名单");
        assert_eq!(
            result.rule_key,
            Some(TranslationComponent::new("BTN_ABILITY_IP_ALLOWLIST_RULE"))
        );
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::with_params(
                "BTN_ABILITY_IP_ALLOWLIST_HIT",
                vec![Param::Text(" 白名单".to_string())],
            ))
        );

        // 黑名单命中 ⇒ BAN（注释作为 reason 参数）
        let result = check(
            &module,
            &peer("2.2.2.3", 51413, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Ban);
        assert_eq!(result.ban_duration_ms, BTN_BAN_DURATION_MS);
        assert_eq!(result.data["type"], "ip");
        assert_eq!(
            result.rule_key,
            Some(TranslationComponent::new("BTN_ABILITY_IP_DENYLIST_RULE"))
        );
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::with_params(
                "BTN_ABILITY_IP_DENYLIST_HIT",
                vec![Param::Text(" 恶意网段".to_string())],
            ))
        );
    }

    /// 现代协议的 IP 黑名单路径没有握手闸门（对齐上游 `checkShouldBanModern`）
    #[test]
    fn modern_denylist_is_not_gated_by_handshake() {
        let module = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        module.apply_ip_denylist_text("2.2.2.0/24\n", "v1");
        let result = check(
            &module,
            &handshaking_peer("2.2.2.2", 51413, "-qb0000-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::Ban);
    }

    /// 白名单更新后，已封禁且命中白名单的地址被解封（对齐 `@Subscribe onRuleUpdate`）
    #[test]
    fn rule_update_unbans_allowlisted_addresses() {
        let module = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        let ban_list = Arc::new(StdMutex::new(BanList::new()));
        {
            let mut list = ban_list.lock().unwrap();
            list.add("2.2.2.2", 0, "btn", false);
            list.add("9.9.9.9", 0, "btn", false);
        }
        // 白名单未注入时不做任何事
        module.on_rule_update(&ban_list);
        assert_eq!(ban_list.lock().unwrap().len(), 2);

        module.apply_ip_allowlist_text("2.2.2.2\n", "v1");
        module.on_rule_update(&ban_list);
        let list = ban_list.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains("9.9.9.9"));
    }

    /// 非法规则集不得破坏已加载的旧规则集（对齐上游异常后保留旧 `btnRule`）
    #[test]
    fn malformed_ruleset_keeps_previous_state() {
        let module = module_with_ruleset(
            r#"{"version":"v1","peer_id":{"xunlei":["{\"method\":\"STARTS_WITH\",\"content\":\"-hp\"}"]}}"#,
        );
        assert_eq!(module.ruleset_version().as_deref(), Some("v1"));

        assert!(module.apply_ruleset_json("not a json").is_err());
        assert!(module
            .apply_ruleset_json(r#"{"version":"v2","peer_id":{"xunlei":["not a rule json"]}}"#)
            .is_err());
        assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
        assert_eq!(
            check(
                &module,
                &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent")
            )
            .action,
            PeerAction::Ban
        );
    }

    /// `unload()` 清空全部规则（对齐 `BtnNetwork.resetAbilities()`）⇒ 恢复为 pass
    #[test]
    fn unload_clears_all_loaded_rules() {
        let module = module_with_ruleset(
            r#"{"version":"v1","peer_id":{"xunlei":["{\"method\":\"STARTS_WITH\",\"content\":\"-hp\"}"]}}"#,
        );
        assert_eq!(
            check(
                &module,
                &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent")
            )
            .action,
            PeerAction::Ban
        );
        module.unload();
        assert!(!module.is_manager_initialized());
        assert_eq!(module.ruleset_version(), None);
        let result = check(
            &module,
            &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.data["status"], "pass");
    }

    /// 规则集解析：版本缺失回退 `initial`，`size()` 统计五类规则
    #[test]
    fn ruleset_parses_and_counts_all_rule_kinds() {
        let ruleset = BtnRuleset {
            version: None,
            peer_id_rules: BTreeMap::from([(
                "a".to_string(),
                vec![r#"{"method":"STARTS_WITH","content":"-hp"}"#.to_string()],
            )]),
            client_name_rules: BTreeMap::from([(
                "b".to_string(),
                vec![r#"{"method":"CONTAINS","content":"xunlei"}"#.to_string()],
            )]),
            ip_rules: BTreeMap::from([("c".to_string(), vec!["1.2.3.0/24".to_string()])]),
            port_rules: BTreeMap::from([("d".to_string(), vec![51413])]),
            script_rules: BTreeMap::from([("e".to_string(), "return true".to_string())]),
        };
        assert_eq!(ruleset.version_or_initial(), "initial");
        let parsed = BtnRulesetParsed::parse(&ruleset).expect("规则集解析");
        assert_eq!(parsed.version, "initial");
        assert_eq!(parsed.peer_id_rule_count(), 1);
        assert_eq!(parsed.client_name_rule_count(), 1);
        assert_eq!(parsed.ip_rule_count(), 1);
        assert_eq!(parsed.port_rule_count(), 1);
        assert_eq!(parsed.script_rule_count(), 1);
        assert_eq!(parsed.size(), 5);

        // IP 匹配器：最长前缀优先
        let list = BtnIpList::from_cidrs(
            "test",
            "v1",
            &[
                "1.0.0.0/8".to_string(),
                "1.2.3.0/24".to_string(),
                "bad".to_string(),
            ],
        );
        assert_eq!(list.len(), 2);
        assert!(list.match_ip("1.2.3.4").is_some());
        assert!(list.match_ip("1.9.9.9").is_some());
        assert!(list.match_ip("8.8.8.8").is_none());
        assert!(list.match_ip("not-an-ip").is_none());
    }

    /// 传输层不可达（无 BTN 服务器 / 网络故障）⇒ 恒 pass；已有规则不被清空（优雅降级）
    #[test]
    fn unreachable_transport_never_bans_and_keeps_rules() {
        struct FailingTransport;
        impl BtnTransport for FailingTransport {
            fn fetch_ruleset(&self, _rev: &str) -> anyhow::Result<Option<BtnRuleset>> {
                anyhow::bail!("connection refused")
            }
            fn fetch_ip_list(
                &self,
                _kind: BtnIpAbility,
                _rev: &str,
            ) -> anyhow::Result<Option<(String, String)>> {
                anyhow::bail!("connection refused")
            }
        }

        // 未配置/不可达：模块保持未初始化，恒 pass
        let module = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        assert!(!module.sync_from_transport(&FailingTransport));
        assert!(!module.is_manager_initialized());
        let result = check(
            &module,
            &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "Xunlei"),
        );
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.data["status"], "pass");

        // 已有规则后拉取失败：保留旧规则继续判定
        let module = module_with_ruleset(
            r#"{"version":"v1","peer_id":{"xunlei":["{\"method\":\"STARTS_WITH\",\"content\":\"-hp\"}"]}}"#,
        );
        assert!(!module.sync_from_transport(&FailingTransport));
        assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
        assert_eq!(
            check(
                &module,
                &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "Xunlei")
            )
            .action,
            PeerAction::Ban
        );
    }

    /// 传输层可用：规则集更新、HTTP 204（`rev` 命中）表示无变化
    #[test]
    fn transport_sync_updates_ruleset_and_handles_204() {
        struct FakeTransport(BtnRuleset);
        impl BtnTransport for FakeTransport {
            fn fetch_ruleset(&self, rev: &str) -> anyhow::Result<Option<BtnRuleset>> {
                if rev == "v1" {
                    return Ok(None);
                }
                Ok(Some(self.0.clone()))
            }
            fn fetch_ip_list(
                &self,
                kind: BtnIpAbility,
                _rev: &str,
            ) -> anyhow::Result<Option<(String, String)>> {
                match kind {
                    // 白名单无内容（上游 204）
                    BtnIpAbility::AllowList => Ok(None),
                    BtnIpAbility::DenyList => {
                        Ok(Some(("2.2.2.0/24 # 恶意\n".to_string(), "v1".to_string())))
                    }
                }
            }
        }

        let ruleset = BtnRuleset {
            version: Some("v1".to_string()),
            peer_id_rules: BTreeMap::from([(
                "xunlei".to_string(),
                vec![r#"{"method":"STARTS_WITH","content":"-hp"}"#.to_string()],
            )]),
            ..Default::default()
        };
        let module = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        assert!(module.sync_from_transport(&FakeTransport(ruleset.clone())));
        assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
        assert_eq!(
            check(
                &module,
                &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "Xunlei")
            )
            .action,
            PeerAction::Ban
        );
        // 黑名单已随同注入
        assert_eq!(
            check(
                &module,
                &peer("2.2.2.9", 12345, "-qb0000-abcdefghijkl", "qBittorrent")
            )
            .action,
            PeerAction::Ban
        );

        // rev 命中 => 上游 204，不重新注入
        assert!(!module.sync_from_transport(&FakeTransport(ruleset)));
        assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
    }

    /// 上游 `BTN_MANAGER_NOT_INITIALIZED` 的等价形态（同样是 NO_ACTION、绝不封禁）
    #[test]
    fn btn_manager_not_initialized_result_matches_upstream() {
        let result = btn_manager_not_initialized_result("btn");
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.ban_duration_ms, 0);
        assert_eq!(result.data["status"], "btn_manager_not_initialized");
        assert_eq!(
            result.rule_key,
            Some(TranslationComponent::new("GENERAL_NA"))
        );
        assert_eq!(
            result.reason_key,
            Some(TranslationComponent::new("BtnManager not initialized"))
        );
    }

    #[test]
    fn module_metadata_matches_upstream() {
        let module = BtnNetworkOnline::new(BTN_BAN_DURATION_MS);
        assert_eq!(module.name(), "BTN Network Online Rules");
        assert_eq!(module.config_name(), "btn");
    }
}
