//! 规则引擎模块 `expression-engine`（对齐上游 `ExpressionRule`，AviatorScript → rhai）。
//!
//! 契约见 SPEC 第 5 节与上游 `ExpressionRule.java` / `ScriptEngineManager.kt`：
//! - 脚本文件取自 `<data>/scripts/` 目录（仅 `.av` 扩展名，对齐 AviatorScript 文件类型）；
//! - 默认该目录为空 → 模块恒返回 `pass()`（与上游空脚本目录行为一致，不改变任何封禁决策）；
//! - 每条脚本向 AviatorScript 注入 `peer` / `torrent` / `downloader` / `banDuration` 等环境变量，
//!   返回值按 `ScriptEngineManager.handleResult` 的语义映射为 `CheckResult`；
//! - 单条脚本超时（上游 1500ms）或抛异常 → 视为 `pass()`（上游 `runExpression` 的兜底）；
//! - 多条脚本聚合（对齐上游 `shouldBanPeer`）：`SKIP` 优先级最高（命中即短路），
//!   其次 `BAN`，否则 `pass()`。
//!
//! 注：上游使用 AviatorScript（JVM），本忠实重写以 `rhai` 作为等价脚本引擎，
//! 并在加载时经 [`crate::avscript::transpile`] 把 AviatorScript **自动翻译**为 rhai——
//! 上游社区脚本（`.av`）可原样使用；仅翻译不了的构造（三元、`=~`、`string.split` 等）
//! 会被跳过并记日志。默认空目录的无脚本行为与上游一致。

use crate::avscript::{build_script_env, transpile, ScriptDownloader};
use crate::i18n::TranslationComponent;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, PeerAction, RuleModule};
use rhai::Engine;
use rhai::Scope;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// 注入脚本的环境变量名。
const MODULE_NAME: &str = "expression-engine";

/// 编译后的单条脚本。
struct LoadedScript {
    name: String,
    ast: rhai::AST,
}

/// 表达式引擎模块。
pub struct ExpressionEngine {
    ban_duration_ms: i64,
    /// 共享 rhai 引擎（已注册自定义类型与超时回调）。
    engine: Engine,
    /// 超时判定用的起始时间戳（原子，便于 `on_progress` 在每次脚本执行前写入）。
    /// 并发多 peer 执行时该值可能被交错覆盖，仅作安全上界，非精确计时。
    start: Arc<AtomicI64>,
    scripts: Vec<LoadedScript>,
}

/// 解析脚本目录：显式配置优先；其次 `PBH_DATA_DIR/scripts`；最后回退 `data/scripts`。
fn resolve_scripts_dir(explicit: Option<&str>) -> PathBuf {
    if let Some(d) = explicit {
        return PathBuf::from(d);
    }
    if let Ok(data) = std::env::var("PBH_DATA_DIR") {
        return PathBuf::from(data).join("scripts");
    }
    PathBuf::from("data").join("scripts")
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 构造 rhai 引擎：注册 `peer` / `torrent` / `downloader` 等自定义类型与超时回调。
///
/// 类型注册与属性暴露统一在 [`crate::avscript::build_script_env`]：同时提供上游驼峰
/// （`peer.clientName` / `peer.peerAddress.address` / `torrent.completedSize` 等）
/// 与 snake_case（`peer.client_name` 等）两套 getter，`#[derive(CustomType)]` 的
/// 自动 getter 对普通字段照常生效（如 `peer.raw_ip`）。
fn build_engine() -> (Engine, Arc<AtomicI64>) {
    let env = build_script_env();
    (env.engine, env.start)
}

/// 解析脚本头部元数据（`## @NAME` / `@AUTHOR` / `@CACHEABLE` / `@VERSION` / `@THREADSAFE`）。
///
/// 对齐上游 `AVScriptEngine.compileScript`：`#` 开头的行剥去井号后识别 `@NAME` 等
/// （上游按 `substring(2)` 处理，即社区脚本惯用的 `##` 双井号；这里对 1 个或多个 `#` 都兼容）。
fn parse_metadata(source: &str) -> (String, bool) {
    let mut name = String::new();
    let mut cacheable = true;
    for line in source.lines() {
        let Some(rest) = line.trim_start().strip_prefix('#') else {
            continue;
        };
        let rest = rest.trim_start_matches('#').trim();
        let Some(body) = rest.strip_prefix('@') else {
            continue;
        };
        let body = body.trim();
        if let Some(v) = body.strip_prefix("NAME") {
            name = v.trim().to_string();
        } else if let Some(v) = body.strip_prefix("CACHEABLE") {
            cacheable = v.trim().parse().unwrap_or(true);
        } else if body.strip_prefix("AUTHOR").is_some()
            || body.strip_prefix("VERSION").is_some()
            || body.strip_prefix("THREADSAFE").is_some()
        {
            // 仅 @NAME 用于展示名；其余字段本实现忽略（不影响封禁语义）
        }
    }
    (name, cacheable)
}

/// 从目录加载所有 `.av` 脚本（对齐 AviatorScript 文件类型）。
///
/// 源码先经 [`crate::avscript::transpile`] 从 AviatorScript 翻译为 rhai 再编译——
/// 上游社区脚本（PBH-BTN 规则）可原样放入目录，无需手写 rhai。
fn load_scripts(engine: &Engine, dir: &std::path::Path) -> Vec<LoadedScript> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out; // 目录不存在 → 无脚本（默认行为）
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext.eq_ignore_ascii_case("av") {
            if let Ok(source) = std::fs::read_to_string(&path) {
                let (mut name, _cacheable) = parse_metadata(&source);
                if name.is_empty() {
                    name = path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                }
                let translated = match transpile(&source) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(
                            "表达式脚本 {name} 的 AviatorScript 含暂不支持的语法（{e}），已跳过"
                        );
                        continue;
                    }
                };
                match engine.compile(&translated) {
                    Ok(ast) => out.push(LoadedScript { name, ast }),
                    Err(e) => tracing::warn!("表达式脚本 {name} 编译失败，已跳过: {e}"),
                }
            }
        }
    }
    out
}

impl ExpressionEngine {
    pub fn new(ban_duration_ms: i64, scripts_dir: Option<&str>) -> Self {
        let (engine, start) = build_engine();
        let dir = resolve_scripts_dir(scripts_dir);
        let scripts = load_scripts(&engine, &dir);
        Self {
            ban_duration_ms,
            engine,
            start,
            scripts,
        }
    }

    /// 将脚本返回值按上游 `ScriptEngineManager.handleResult` 的语义映射为决策。
    /// 返回 `None` 表示 `pass()`（无动作）。
    fn handle_return(&self, ret: &rhai::Dynamic) -> Option<(PeerAction, String)> {
        // Boolean：true → BAN；false → pass
        if ret.is_bool() {
            return if ret.as_bool().unwrap_or(false) {
                Some((PeerAction::Ban, "true".to_string()))
            } else {
                None
            };
        }
        // Number：0 → pass；1 → BAN；2 → SKIP；其它 → pass（上游记日志后视为无效）
        if ret.is_int() {
            match ret.as_int().unwrap_or(0) {
                0 => return None,
                1 => return Some((PeerAction::Ban, "1".to_string())),
                2 => return Some((PeerAction::Skip, "2".to_string())),
                _ => return None,
            }
        }
        if ret.is_float() {
            let v = ret.as_float().unwrap_or(0.0).round() as i64;
            return match v {
                0 => None,
                1 => Some((PeerAction::Ban, v.to_string())),
                2 => Some((PeerAction::Skip, v.to_string())),
                _ => None,
            };
        }
        // String：空白 → pass；`@` 开头 → SKIP（reason 为去掉 `@` 的原文）；其它 → BAN
        if let Ok(s) = ret.clone().into_string() {
            if s.trim().is_empty() {
                return None;
            }
            if let Some(rest) = s.strip_prefix('@') {
                return Some((PeerAction::Skip, rest.to_string()));
            }
            return Some((PeerAction::Ban, s));
        }
        // 其它类型（含上游的 PeerAction / CheckResult 返回）→ 视作无效，pass
        None
    }

    fn build_result(&self, action: PeerAction, name: &str, payload: &str) -> CheckResult {
        match action {
            PeerAction::Skip => {
                // 对齐上游 `TranslationComponent(returns.substring(1))` 作为 reason 键（原文即键）
                let reason = payload.to_string();
                CheckResult {
                    module: MODULE_NAME.to_string(),
                    action: PeerAction::Skip,
                    ban_duration_ms: 0,
                    rule: MODULE_NAME.to_string(),
                    reason: reason.clone(),
                    data: serde_json::json!({ "script": name }),
                    rule_key: Some(TranslationComponent::new("USER_SCRIPT_RULE")),
                    reason_key: Some(TranslationComponent::new(reason)),
                }
            }
            PeerAction::Ban => CheckResult::ban(
                MODULE_NAME,
                self.ban_duration_ms,
                MODULE_NAME,
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
            _ => CheckResult::pass(MODULE_NAME),
        }
    }

    /// 执行单条脚本；异常或超时 → pass（对齐上游 `runExpression` 兜底）。
    fn run_script(
        &self,
        script: &LoadedScript,
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
        scope.push_constant("banDuration", self.ban_duration_ms);
        scope.push_constant("cacheable", true);
        scope.push_constant("ramStorage", rhai::Dynamic::from(rhai::Map::new()));
        scope.push_constant("moduleInstance", MODULE_NAME.to_string());
        scope.push_constant("server", true);

        self.start.store(now_millis(), Ordering::Relaxed);
        let result = self.engine.eval_ast_with_scope(&mut scope, &script.ast);
        self.start.store(0, Ordering::Relaxed);

        match result {
            Ok(ret) => {
                let (action, payload) = self.handle_return(&ret)?;
                Some(self.build_result(action, &script.name, &payload))
            }
            Err(e) => {
                tracing::debug!("表达式脚本 {} 执行异常，按 pass 处理: {e}", script.name);
                None
            }
        }
    }
}

impl RuleModule for ExpressionEngine {
    fn name(&self) -> &str {
        "Expression Engine"
    }
    fn config_name(&self) -> &str {
        MODULE_NAME
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
        // 对齐上游 shouldBanPeer：SKIP 优先（短路），其次 BAN，否则 pass
        let mut decided: Option<CheckResult> = None;
        for script in &self.scripts {
            let Some(result) = self.run_script(script, torrent, peer, downloader_id) else {
                continue;
            };
            match result.action {
                PeerAction::Skip => return result, // 短路，SKIP 优先级最高
                PeerAction::Ban
                    if decided
                        .as_ref()
                        .map(|d| d.action)
                        .unwrap_or(PeerAction::NoAction)
                        != PeerAction::Skip =>
                {
                    decided = Some(result);
                }
                _ => {}
            }
        }
        decided.unwrap_or_else(|| CheckResult::pass(MODULE_NAME))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::CheckContext;
    use std::fs;

    fn peer() -> PeerData {
        PeerData {
            client_name: Some("qBittorrent/4.3.9".to_string()),
            peer_id: Some("-qB4390-".to_string()),
            dl_speed: 0,
            downloaded: 0,
            up_speed: 0,
            uploaded: 0,
            progress: 0.5,
            flags: Some("".to_string()),
            ip: "1.2.3.4".to_string(),
            port: 12345,
            raw_ip: "1.2.3.4:12345".to_string(),
            connection: None,
        }
    }

    fn torrent() -> TorrentData {
        TorrentData {
            hash: "abc".to_string(),
            name: "t".to_string(),
            progress: 0.5,
            total_size: 1000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        }
    }

    fn ctx() -> CheckContext {
        CheckContext::default()
    }

    /// 在临时目录写入脚本并构建模块。每次调用使用独立目录，避免并发测试互相覆盖。
    fn with_script(script: &str, body: &str) -> ExpressionEngine {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("pbh_expr_test_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(script), body).unwrap();
        ExpressionEngine::new(0, Some(dir.to_str().unwrap()))
    }

    fn result_of(m: &ExpressionEngine) -> CheckResult {
        m.check("qb", &torrent(), &peer(), &ctx())
    }

    #[test]
    fn empty_directory_is_noop() {
        let dir = std::env::temp_dir().join(format!("pbh_expr_empty_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
        assert_eq!(result_of(&m).action, PeerAction::NoAction);
    }

    #[test]
    fn boolean_true_bans() {
        let m = with_script("a.av", "true");
        let r = result_of(&m);
        assert_eq!(r.action, PeerAction::Ban);
        assert_eq!(r.rule_key.as_ref().unwrap().key, "USER_SCRIPT_RULE");
    }

    #[test]
    fn boolean_false_passes() {
        let m = with_script("a.av", "false");
        assert_eq!(result_of(&m).action, PeerAction::NoAction);
    }

    #[test]
    fn number_one_bans_two_skips() {
        let m = with_script("a.av", "1");
        assert_eq!(result_of(&m).action, PeerAction::Ban);
        let m = with_script("b.av", "2");
        assert_eq!(result_of(&m).action, PeerAction::Skip);
    }

    #[test]
    fn string_blank_passes_at_prefix_skips() {
        let m = with_script("a.av", "\"\"");
        assert_eq!(result_of(&m).action, PeerAction::NoAction);
        let m = with_script("b.av", "\"@custom-reason\"");
        let r = result_of(&m);
        assert_eq!(r.action, PeerAction::Skip);
        // `@` 之后的原文作为 reason 键
        assert_eq!(r.reason_key.as_ref().unwrap().key, "custom-reason");
    }

    #[test]
    fn string_nonblank_bans_with_reason() {
        let m = with_script("a.av", "\"ban this peer\"");
        let r = result_of(&m);
        assert_eq!(r.action, PeerAction::Ban);
        assert_eq!(r.reason_key.as_ref().unwrap().key, "USER_SCRIPT_RUN_RESULT");
    }

    #[test]
    fn bad_syntax_is_skipped_noop() {
        // 编译失败 → 无脚本 → pass
        let dir = std::env::temp_dir().join(format!("pbh_expr_bad_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x.av"), "let x = ;").unwrap();
        let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
        assert_eq!(result_of(&m).action, PeerAction::NoAction);
    }

    #[test]
    fn skip_takes_priority_over_ban() {
        let dir = std::env::temp_dir().join(format!("pbh_expr_agg_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 两条脚本：一条返回 1（BAN），一条返回 2（SKIP）
        fs::write(dir.join("ban.av"), "1").unwrap();
        fs::write(dir.join("skip.av"), "2").unwrap();
        let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
        // SKIP 优先级最高（上游短路返回）
        assert_eq!(result_of(&m).action, PeerAction::Skip);
    }

    #[test]
    fn script_can_read_peer_fields() {
        // peer.ip 等于 "1.2.3.4" → 返回 1 封禁（验证自定义类型绑定）
        let m = with_script("a.av", "if peer.ip == \"1.2.3.4\" { 1 } else { 0 }");
        assert_eq!(result_of(&m).action, PeerAction::Ban);
    }
}
