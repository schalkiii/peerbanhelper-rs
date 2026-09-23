//! AviatorScript 兼容层（对齐上游 `AVScriptEngine` + AviatorScript 内建函数）。
//!
//! 上游的表达式规则（`expression-engine` 的 `<data>/scripts/*.av` 与 BTN 的 `scriptRules`）
//! 都是 **AviatorScript** 语法；本移植以 rhai 执行脚本，因此在此提供两层兼容：
//!
//! 1. [`transpile`]：把 AviatorScript 源码翻译为 rhai 源码（词法级扫描，字符串/注释感知）：
//!    - `##` 行注释 → `//`；
//!    - 单引号字符串 → 双引号（转义改写）；
//!    - 内建函数改写：`string.{contains,startsWith,endsWith,indexOf,trim,length,substring}` →
//!      等价 rhai 字符串方法；`toLowerCase`/`toUpperCase`/`toString` → 方法形式；
//!      `isBlank`/`isNotBlank` → 注册的自定义函数；
//!    - `seq.map(k1,v1,…)` → **扁平数组**字面量（不翻译成 rhai 的 `#{}` map——rhai 的 Map
//!      是按键排序的 BTreeMap，而 Aviator 保持**插入序**；社区脚本 `name-id-verify.av`
//!      依赖 `'aria2explorer'` 先于 `'aria2'` 被前缀匹配，排序会改变判定结果），
//!      `seq.keys(m)`/`seq.get(m,k)` → 保序访问函数 [`fn@AV_KEYS`]/[`fn@AV_GET`]（注册名
//!      `av_keys`/`av_get`）；`seq.list(…)` → 数组字面量；
//!    - 语句级裸赋值 `x = …` → `let x = …`（rhai 允许 `let` 重声明遮蔽）；
//!    - `nil` → `()`。
//!    翻译不了的构造（三元、正则 `=~`、`string.split` 等）返回 `Err`，由调用方记日志跳过
//!    （对齐上游「编译失败 → 跳过该脚本」）。
//!
//! 2. [`build_script_env`]：共享的 rhai 引擎构造。同时暴露 **snake_case 与上游驼峰**
//!    两套属性 getter（`peer.clientName` / `peer.client_name` 等），并补齐上游
//!    `Peer`/`Torrent` 接口有而此前缺失的字段：`peer.peerAddress.{ip,port,address}`
//!    （`address` 对齐 `IPAddressUtil.getIPAddress(ip).toPrefixBlock().toString()`——
//!    peer IP 是无前缀单地址，`toPrefixBlock()` 原样返回自身，即**规范化地址串**）、
//!    `torrent.completedSize`、`torrent.private`、`torrent.seeding`、`torrent.hashedIdentifier`。
//!
//! 返回值语义（bool/数字/字符串 → BAN/SKIP/pass）与超时兜底由使用方
//! （[`crate::modules::expression_engine`] 与 [`crate::modules::btn`]）实现，与上游
//! `handleResult` 逐项一致（见 `docs/expression-engine-migration.md` §6）。

use crate::model::{PeerData, TorrentData};
use rhai::CustomType;
use rhai::Engine;
use rhai::Position;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// 上游 `ExpressionRule.maxScriptExecuteTime = 1500`（毫秒）。
pub const MAX_SCRIPT_EXECUTE_MS: i64 = 1500;

/// 注入脚本的 `downloader` 变量（上游 `Downloader` 接口只有 id/name）。
#[derive(Debug, Clone, CustomType)]
pub struct ScriptDownloader {
    pub id: String,
    pub name: String,
}

/// 注入脚本的 `peer.peerAddress`（上游 `PeerAddress` 包装器的 Aviator 可见属性）。
///
/// `address` 是延迟计算的规范化地址串：对齐上游 `getAddress()` =
/// `IPAddressUtil.getIPAddress(ip).toPrefixBlock()`——peer IP 无前缀时 `toPrefixBlock()`
/// 返回自身，故其 `toString()` 就是「去方括号 + IPv4-mapped 归一 + 规范写法」的地址串。
#[derive(Debug, Clone, CustomType)]
pub struct PeerAddressInfo {
    ip: String,
    port: i64,
}

fn canonical_address(ip: &str) -> String {
    crate::iputil::parse_addr(ip)
        .map(|a| a.to_string())
        .unwrap_or_else(|| ip.to_string())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 共享脚本环境：编译好的引擎 + 超时判定的时间戳。
pub struct ScriptEnv {
    pub engine: Engine,
    /// 每次脚本执行前由调用方写入起始时间戳（`on_progress` 据此中止超时脚本）。
    pub start: Arc<AtomicI64>,
}

/// 构造共享 rhai 引擎：上游 `Peer`/`Torrent`/`Downloader`/`PeerAddress` 的全部 Aviator
/// 可见属性（驼峰 + 既有 snake_case 双套）、`isBlank` 等内建函数与 1500ms 超时回调。
pub fn build_script_env() -> ScriptEnv {
    let start = Arc::new(AtomicI64::new(0));
    let mut engine = Engine::new();

    // ---- peer：snake_case（历史写法）+ 上游驼峰（AviatorScript JavaBean getter 名） ----
    engine.register_type_with_name::<PeerData>("Peer");
    engine.register_get("ip", |o: &mut PeerData| o.ip.clone());
    engine.register_get("port", |o: &mut PeerData| o.port as i64);
    engine.register_get("peer_id", |o: &mut PeerData| {
        o.peer_id.clone().unwrap_or_default()
    });
    engine.register_get("client_name", |o: &mut PeerData| {
        o.client_name.clone().unwrap_or_default()
    });
    engine.register_get("download_speed", |o: &mut PeerData| o.dl_speed);
    engine.register_get("upload_speed", |o: &mut PeerData| o.up_speed);
    engine.register_get("downloaded", |o: &mut PeerData| o.downloaded);
    engine.register_get("uploaded", |o: &mut PeerData| o.uploaded);
    engine.register_get("progress", |o: &mut PeerData| o.progress);
    engine.register_get("flags", |o: &mut PeerData| {
        o.flags.clone().unwrap_or_default()
    });
    // 驼峰别名（对齐上游 JavaBean getter：getPeerId/getClientName/…）
    engine.register_get("peerId", |o: &mut PeerData| {
        o.peer_id.clone().unwrap_or_default()
    });
    engine.register_get("clientName", |o: &mut PeerData| {
        o.client_name.clone().unwrap_or_default()
    });
    engine.register_get("downloadSpeed", |o: &mut PeerData| o.dl_speed);
    engine.register_get("uploadSpeed", |o: &mut PeerData| o.up_speed);
    engine.register_get("peerAddress", |o: &mut PeerData| PeerAddressInfo {
        ip: o.ip.clone(),
        port: o.port as i64,
    });

    // ---- peer.peerAddress：ip / port / address（规范化地址串，见类型文档） ----
    engine.register_type_with_name::<PeerAddressInfo>("PeerAddress");
    engine.register_get("ip", |o: &mut PeerAddressInfo| o.ip.clone());
    engine.register_get("port", |o: &mut PeerAddressInfo| o.port);
    engine.register_get("address", |o: &mut PeerAddressInfo| {
        canonical_address(&o.ip)
    });

    // ---- torrent：snake_case + 驼峰 + 上游接口补齐的字段 ----
    engine.register_type_with_name::<TorrentData>("Torrent");
    engine.register_get("id", |o: &mut TorrentData| o.hash.clone());
    engine.register_get("name", |o: &mut TorrentData| o.name.clone());
    engine.register_get("hash", |o: &mut TorrentData| o.hash.clone());
    engine.register_get("progress", |o: &mut TorrentData| o.progress);
    engine.register_get("size", |o: &mut TorrentData| o.total_size);
    engine.register_get("rt_upload_speed", |o: &mut TorrentData| o.upspeed);
    engine.register_get("rt_download_speed", |o: &mut TorrentData| o.dlspeed);
    engine.register_get("rtUploadSpeed", |o: &mut TorrentData| o.upspeed);
    engine.register_get("rtDownloadSpeed", |o: &mut TorrentData| o.dlspeed);
    // 上游 `Torrent#getCompletedSize`（各适配器同一语义，见 TorrentData::completed_size）
    engine.register_get("completedSize", |o: &mut TorrentData| o.completed_size());
    // 上游 `isPrivate()` / `isSeeding()`
    engine.register_get("private", |o: &mut TorrentData| {
        o.is_private.unwrap_or(false)
    });
    engine.register_get("seeding", |o: &mut TorrentData| o.progress >= 1.0);
    engine.register_get("hashedIdentifier", |o: &mut TorrentData| {
        crate::btn_transport::get_hashed_identifier(&o.hash)
    });

    // ---- downloader ----
    engine.register_type_with_name::<ScriptDownloader>("Downloader");
    engine.register_get("id", |o: &mut ScriptDownloader| o.id.clone());
    engine.register_get("name", |o: &mut ScriptDownloader| o.name.clone());

    // ---- Aviator 内建函数（无命名空间者） ----
    // Aviator `isBlank(s)`：s 为 null 时上游抛异常（脚本整体按 pass 兜底）；
    // 这里把 null 映射为 true（空白），最终对空值 peer 字段的判定结果一致。
    engine.register_fn("is_blank", |v: rhai::Dynamic| -> bool {
        if v.is_unit() {
            return true;
        }
        match v.clone().into_string() {
            Ok(s) => s.trim().is_empty(),
            Err(_) => false,
        }
    });
    engine.register_fn("is_not_blank", |v: rhai::Dynamic| -> bool {
        if v.is_unit() {
            return false;
        }
        match v.clone().into_string() {
            Ok(s) => !s.trim().is_empty(),
            Err(_) => false,
        }
    });
    // `seq.map` 的保序访问函数（见模块文档：rhai Map 有序，Aviator 保持插入序）
    engine.register_fn("av_keys", |m: rhai::Array| -> rhai::Array {
        m.chunks(2)
            .filter_map(|pair| pair.first().cloned())
            .collect()
    });
    engine.register_fn(
        "av_get",
        |m: rhai::Array, key: rhai::ImmutableString| -> rhai::Dynamic {
            for pair in m.chunks(2) {
                if pair.len() == 2 && pair[0].to_string() == key.as_str() {
                    return pair[1].clone();
                }
            }
            rhai::Dynamic::UNIT
        },
    );
    // Aviator `double(x)` / `long(x)` / `int(x)`：失败时抛错（脚本按 pass 兜底，
    // 对齐上游 NumberFormatException → 异常路径）
    engine.register_fn(
        "av_double",
        |v: rhai::Dynamic| -> Result<rhai::FLOAT, Box<rhai::EvalAltResult>> {
            if let Ok(f) = v.as_float() {
                return Ok(f);
            }
            if let Ok(i) = v.as_int() {
                return Ok(i as rhai::FLOAT);
            }
            let s = v.clone().into_string().map_err(|_| {
                rhai::EvalAltResult::ErrorRuntime(
                    format!("double() 需要 string，实得 {}", v.type_name()).into(),
                    Position::NONE,
                )
            })?;
            s.trim().parse::<rhai::FLOAT>().map_err(|e| {
                Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("double() 无法解析 {s:?}: {e}").into(),
                    Position::NONE,
                ))
            })
        },
    );
    engine.register_fn(
        "av_long",
        |v: rhai::Dynamic| -> Result<i64, Box<rhai::EvalAltResult>> {
            if let Ok(i) = v.as_int() {
                return Ok(i);
            }
            if let Ok(f) = v.as_float() {
                // 对齐 Java `(long) double` 的向零截断
                return Ok(f as i64);
            }
            let s = v.clone().into_string().map_err(|_| {
                rhai::EvalAltResult::ErrorRuntime(
                    format!("long() 需要 string，实得 {}", v.type_name()).into(),
                    Position::NONE,
                )
            })?;
            s.trim().parse::<i64>().map_err(|e| {
                Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("long() 无法解析 {s:?}: {e}").into(),
                    Position::NONE,
                ))
            })
        },
    );
    // Aviator `'a' + 123` 是合法字符串拼接；rhai 的 String + Int 未注册会运行期失败，
    // 整脚本按 pass 兜底造成漏封。注册跨类型 `+`（任意值按 Java `String.valueOf` 语义转串拼接）。
    engine.register_fn(
        "+",
        |a: rhai::ImmutableString, b: rhai::Dynamic| -> String { format!("{}{}", a, b) },
    );
    engine.register_fn(
        "+",
        |a: rhai::Dynamic, b: rhai::ImmutableString| -> String { format!("{}{}", a, b) },
    );

    // ---- 超时兜底（对齐上游 runExpression 的 maxScriptExecuteTime） ----
    let start_for_cb = start.clone();
    engine.on_progress(move |_progress: u64| {
        let s = start_for_cb.load(Ordering::Relaxed);
        if s == 0 {
            return None;
        }
        if now_millis() - s > MAX_SCRIPT_EXECUTE_MS {
            // 超时：中止脚本执行，返回值 0（上游语义：pass）
            Some(rhai::Dynamic::from(0))
        } else {
            None
        }
    });

    ScriptEnv { engine, start }
}

// ============================================================================
// AviatorScript → rhai 翻译器
// ============================================================================

/// 解析脚本头部元数据（`## @NAME` / `@AUTHOR` / `@CACHEABLE` / `@VERSION` / `@THREADSAFE`）。
///
/// 对齐上游 `AVScriptEngine.compileScript`：`#` 开头的行剥去井号后识别 `@NAME` 等
/// （上游按 `substring(2)` 处理，即社区脚本惯用的 `##` 双井号；这里对 1 个或多个 `#` 都兼容）。
pub fn parse_metadata(source: &str) -> (String, bool) {
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
            // 仅 @NAME 用于展示名；其余字段不影响封禁语义
        }
    }
    (name, cacheable)
}

/// 把 AviatorScript 源码翻译为 rhai 源码；无法安全翻译时返回 `Err(原因)`。
pub fn transpile(source: &str) -> Result<String, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len() + 64);
    let mut i = 0usize;
    let n = chars.len();

    while i < n {
        let c = chars[i];

        // 行注释：AviatorScript 用 `##`（元数据头 `## @NAME` 同规则）→ rhai `//`
        if c == '#' {
            out.push_str("//");
            while i < n && chars[i] == '#' {
                i += 1;
            }
            while i < n && chars[i] != '\n' {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        // 块注释：两种语言都是 `/* */`，原样保留
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let start = i;
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 >= n {
                return Err("块注释未闭合".into());
            }
            i += 2;
            out.push_str(&chars[start..i].iter().collect::<String>());
            continue;
        }

        // 字符串字面量：单引号 → 双引号（转义改写），双引号原样（除 \' 外）
        if c == '\'' || c == '"' {
            let (s, ni) = read_string(&chars, i)?;
            out.push_str(&s);
            i = ni;
            continue;
        }

        // 标识符
        if is_ident_start(c) {
            let start = i;
            while i < n && is_ident_char(chars[i]) {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            // 向前看第一个非空白字符
            let mut j = i;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }

            // 1) 命名空间内建：string.X(..) / seq.X(..)
            if (ident == "string" || ident == "seq") && j < n && chars[j] == '.' {
                let m0 = j + 1;
                let mut m1 = m0;
                while m1 < n && is_ident_char(chars[m1]) {
                    m1 += 1;
                }
                let method: String = chars[m0..m1].iter().collect();
                let mut k = m1;
                while k < n && chars[k].is_whitespace() {
                    k += 1;
                }
                if k < n && chars[k] == '(' {
                    let close = find_matching_paren(&chars, k)?;
                    let args = split_args(&chars, k, close);
                    emit_namespaced_call(&mut out, &chars, &ident, &method, &args)?;
                    i = close + 1;
                    continue;
                }
                // `string.` / `seq.` 后不是调用：原样透传（后续 rhai 编译报错，清晰可见）
                out.push_str(&ident);
                out.push('.');
                i = m0;
                continue;
            }

            // 2) 无命名空间内建函数（控制流关键字不是调用，走下方原样路径）
            if j < n && chars[j] == '(' && !is_keyword(&ident) {
                if let Some(kind) = builtin_kind(&ident) {
                    let close = find_matching_paren(&chars, j)?;
                    let args = split_args(&chars, j, close);
                    emit_builtin_call(&mut out, &chars, kind, &args)?;
                    i = close + 1;
                    continue;
                }
                // 其它函数调用：名字原样保留，参数随主扫描继续（嵌套改写自然生效）
                out.push_str(&ident);
                out.push('(');
                i = j + 1;
                continue;
            }

            // 3) 裸赋值 `x = …`（非 `==`）→ `let x = …`。
            //    Aviator 裸赋值即变量定义，且语句分隔符（换行/分号）可省略，
            //    无法可靠判定「语句起始位置」，故凡非比较的 `=` 一律改写为 `let`
            //    （Aviator 不允许条件内赋值，误加 `let` 无副作用；rhai 允许 let 重声明遮蔽）。
            //    源码本身写的是 `let x = …` 时（前一个已产出 token 是 `let`）不再重复包裹。
            if j < n && chars[j] == '=' && (j + 1 >= n || chars[j + 1] != '=') {
                if !output_ends_with_let_keyword(&out) {
                    out.push_str("let ");
                }
                out.push_str(&ident);
                out.push_str(" = ");
                i = j + 1;
                while i < n && chars[i].is_whitespace() {
                    i += 1;
                }
                continue;
            }

            // 4) nil → ()
            if ident == "nil" {
                out.push_str("()");
                continue;
            }

            // 普通标识符：原样（前瞻时跳过的空白也要原样保留）
            out.push_str(&ident);
            out.extend(chars[i..j].iter());
            i = j;
            continue;
        }

        // 其余字符原样
        out.push(c);
        i += 1;
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq)]
enum Builtin {
    IsBlank,
    IsNotBlank,
    ToLower,
    ToUpper,
    ToStr,
    ToDouble,
    ToLong,
    ToInt,
}

fn builtin_kind(ident: &str) -> Option<Builtin> {
    match ident {
        "isBlank" => Some(Builtin::IsBlank),
        "isNotBlank" => Some(Builtin::IsNotBlank),
        "toLowerCase" => Some(Builtin::ToLower),
        "toUpperCase" => Some(Builtin::ToUpper),
        "toString" | "str" => Some(Builtin::ToStr),
        "double" => Some(Builtin::ToDouble),
        "long" => Some(Builtin::ToLong),
        "int" => Some(Builtin::ToInt),
        _ => None,
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// 已产出文本的末尾（去空白后）是否恰好是独立的 `let` 关键字
/// （用于避免把源码自身的 `let x = …` 重复包裹成 `let let x = …`）。
fn output_ends_with_let_keyword(out: &str) -> bool {
    let t = out.trim_end();
    let word: String = t.chars().rev().take_while(|c| is_ident_char(*c)).collect();
    word.chars().rev().eq("let".chars())
}

/// 两语言共用的控制流/保留字：不能当作函数调用改写（如 Aviator 惯用的 `if(...)` 写法）。
fn is_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "if" | "else"
            | "for"
            | "while"
            | "return"
            | "let"
            | "in"
            | "true"
            | "false"
            | "break"
            | "continue"
            | "loop"
            | "do"
            | "switch"
            | "fn"
            | "throw"
            | "try"
            | "catch"
    )
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 读取一个 Aviator 字符串字面量，返回等价的 rhai 双引号字符串与结束位置（引号后）。
fn read_string(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let quote = chars[start];
    let mut out = String::from("\"");
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            match (quote, next) {
                // 单引号串中的 `\'` → 裸 `'`；双引号串中的 `\'` 同理（rhai 不认 `\'`）
                (_, '\'') => out.push('\''),
                // 单引号串中的 `\"` 与裸 `"` 都需转义
                (_, '"') => {
                    out.push('\\');
                    out.push('"');
                }
                // 其余转义对原样保留（\n \t \\ 等两语言兼容）
                _ => {
                    out.push('\\');
                    out.push(next);
                }
            }
            i += 2;
            continue;
        }
        if c == quote {
            out.push('"');
            return Ok((out, i + 1));
        }
        if c == '"' && quote == '\'' {
            // 单引号串内的裸 `"` 需转义
            out.push('\\');
            out.push('"');
            i += 1;
            continue;
        }
        if c == '\n' {
            return Err("字符串未闭合（行尾）".into());
        }
        out.push(c);
        i += 1;
    }
    Err("字符串未闭合".into())
}

/// `chars[open] == '('`，返回与之匹配的 `)` 下标（字符串/注释/嵌套括号感知）。
fn find_matching_paren(chars: &[char], open: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => {
                let (_, ni) = read_string(chars, i)?;
                i = ni;
                continue;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err("括号未闭合".into())
}

/// 把 `(…)` 的顶层实参按逗号切分（返回各实参在 `chars` 中的区间），字符串/嵌套括号感知。
fn split_args(chars: &[char], open: usize, close: usize) -> Vec<(usize, usize)> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    let mut i = open + 1;
    while i < close {
        match chars[i] {
            '\'' | '"' => {
                if let Ok((_, ni)) = read_string(chars, i) {
                    i = ni;
                    continue;
                }
                i += 1;
                continue;
            }
            '#' => {
                while i < close && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < close {
        args.push((start, close));
    }
    args.retain(|(a, b)| chars[*a..*b].iter().any(|c| !c.is_whitespace()));
    args
}

/// 翻译单个实参（去掉首尾空白）。
fn transpile_arg(chars: &[char], range: (usize, usize)) -> Result<String, String> {
    let raw: String = chars[range.0..range.1].iter().collect();
    transpile(&raw).map(|s| s.trim().to_string())
}

/// 改写无命名空间内建调用（实参区间基于 `chars`）。
fn emit_builtin_call(
    out: &mut String,
    chars: &[char],
    kind: Builtin,
    args: &[(usize, usize)],
) -> Result<(), String> {
    if args.is_empty() {
        return Err("内建函数缺少参数".into());
    }
    let a0 = transpile_arg(chars, args[0])?;
    match kind {
        Builtin::IsBlank => {
            require_arg_count("isBlank", args, 1)?;
            out.push_str(&format!("is_blank({a0})"));
        }
        Builtin::IsNotBlank => {
            require_arg_count("isNotBlank", args, 1)?;
            out.push_str(&format!("is_not_blank({a0})"));
        }
        Builtin::ToLower => {
            require_arg_count("toLowerCase", args, 1)?;
            out.push_str(&format!("({a0}).to_lower()"));
        }
        Builtin::ToUpper => {
            require_arg_count("toUpperCase", args, 1)?;
            out.push_str(&format!("({a0}).to_upper()"));
        }
        Builtin::ToStr => {
            require_arg_count("toString", args, 1)?;
            out.push_str(&format!("({a0}).to_string()"));
        }
        Builtin::ToDouble => {
            require_arg_count("double", args, 1)?;
            out.push_str(&format!("av_double({a0})"));
        }
        Builtin::ToLong => {
            require_arg_count("long", args, 1)?;
            out.push_str(&format!("av_long({a0})"));
        }
        Builtin::ToInt => {
            require_arg_count("int", args, 1)?;
            out.push_str(&format!("av_long({a0})"));
        }
    }
    Ok(())
}

/// 改写 `string.X(..)` / `seq.X(..)` 命名空间内建调用（实参区间基于 `chars`）。
fn emit_namespaced_call(
    out: &mut String,
    chars: &[char],
    ns: &str,
    method: &str,
    args: &[(usize, usize)],
) -> Result<(), String> {
    match (ns, method) {
        ("string", "contains") => emit_method_call(out, chars, args, "contains", 2)?,
        ("string", "startsWith") => emit_method_call(out, chars, args, "starts_with", 2)?,
        ("string", "endsWith") => emit_method_call(out, chars, args, "ends_with", 2)?,
        ("string", "indexOf") => emit_method_call(out, chars, args, "index_of", 2)?,
        ("string", "trim") => emit_method_call(out, chars, args, "trim", 1)?,
        ("string", "length") => emit_method_call(out, chars, args, "len", 1)?,
        ("string", "substring") => {
            match args.len() {
                2 => {
                    let a0 = transpile_arg(chars, args[0])?;
                    let a1 = transpile_arg(chars, args[1])?;
                    out.push_str(&format!("({a0}).sub_string({a1})"));
                }
                3 => {
                    let a0 = transpile_arg(chars, args[0])?;
                    let a1 = transpile_arg(chars, args[1])?;
                    let a2 = transpile_arg(chars, args[2])?;
                    // Java `substring(begin, end)` 左闭右开；rhai `sub_string(start, len)`
                    out.push_str(&format!("({a0}).sub_string({a1}, ({a2})-({a1}))"));
                }
                _ => return Err("string.substring 需要 2 或 3 个参数".into()),
            }
        }
        // seq.map → 扁平数组（保插入序，见模块文档）
        ("seq", "map") => {
            let mut parts = Vec::with_capacity(args.len());
            for r in args {
                parts.push(transpile_arg(chars, *r)?);
            }
            out.push('[');
            out.push_str(&parts.join(", "));
            out.push(']');
        }
        ("seq", "keys") => {
            require_arg_count("seq.keys", args, 1)?;
            let a0 = transpile_arg(chars, args[0])?;
            out.push_str(&format!("av_keys({a0})"));
        }
        ("seq", "get") => {
            require_arg_count("seq.get", args, 2)?;
            let a0 = transpile_arg(chars, args[0])?;
            let a1 = transpile_arg(chars, args[1])?;
            out.push_str(&format!("av_get({a0}, {a1})"));
        }
        ("seq", "list") => {
            let mut parts = Vec::with_capacity(args.len());
            for r in args {
                parts.push(transpile_arg(chars, *r)?);
            }
            out.push('[');
            out.push_str(&parts.join(", "));
            out.push(']');
        }
        _ => {
            return Err(format!(
                "暂不支持的 Aviator 内建: {ns}.{method}（请改写为 rhai 等价写法）"
            ));
        }
    }
    Ok(())
}

/// 把 `X(a0, a1, …)` 改写为方法形式 `(a0).m(a1, …)`，并校验实参个数。
fn emit_method_call(
    out: &mut String,
    chars: &[char],
    args: &[(usize, usize)],
    method: &str,
    total_args: usize,
) -> Result<(), String> {
    require_arg_count(method, args, total_args)?;
    let a0 = transpile_arg(chars, args[0])?;
    out.push('(');
    out.push_str(&a0);
    out.push_str(&format!(").{method}("));
    let rest: Vec<String> = args[1..]
        .iter()
        .map(|r| transpile_arg(chars, *r))
        .collect::<Result<_, _>>()?;
    out.push_str(&rest.join(", "));
    out.push(')');
    Ok(())
}

fn require_arg_count(name: &str, args: &[(usize, usize)], expected: usize) -> Result<(), String> {
    if args.len() != expected {
        return Err(format!(
            "{name} 需要 {expected} 个参数，实际 {}",
            args.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transpiled(script: &str) -> String {
        transpile(script).expect("transpile 失败")
    }

    fn eval_string(script: &str) -> String {
        let env = build_script_env();
        env.engine
            .eval::<String>(&transpiled(script))
            .expect("rhai 执行失败")
    }

    fn eval_array(script: &str) -> rhai::Array {
        let env = build_script_env();
        env.engine
            .eval::<rhai::Array>(&transpiled(script))
            .expect("rhai 执行失败")
    }

    // ---------- 翻译器：注释 / 字符串 / 裸赋值 / nil ----------

    #[test]
    fn transpile_hash_comments_and_metadata_lines() {
        let out = transpile("## @NAME 测试\n## 普通注释\nlet x = 1;\nx").unwrap();
        assert_eq!(out, "// @NAME 测试\n// 普通注释\nlet x = 1;\nx");
    }

    #[test]
    fn transpile_single_quoted_strings_to_double() {
        let out = transpile("return '-gp';").unwrap();
        assert_eq!(out, "return \"-gp\";");
        // 单引号串内的裸双引号需转义；双引号串内的单引号去掉转义
        assert_eq!(transpile("return 'a\"b';").unwrap(), "return \"a\\\"b\";");
        assert_eq!(transpile("return \"a\\'b\";").unwrap(), "return \"a'b\";");
        assert_eq!(transpile("return 'a\\nb';").unwrap(), "return \"a\\nb\";");
    }

    #[test]
    fn transpile_bare_assignment_becomes_let() {
        let out = transpile("ipAddress = peer.peerAddress.address;\nstrIp = toString(ipAddress);")
            .unwrap();
        assert_eq!(
            out,
            "let ipAddress = peer.peerAddress.address;\nlet strIp = (ipAddress).to_string();"
        );
    }

    #[test]
    fn transpile_bare_assignment_without_semicolons_still_becomes_let() {
        // Aviator 的语句分隔符可省略（换行即分隔）；上一行以 `)` 结尾时
        // 下一行裸赋值仍必须得到 `let`，否则 rhai 报未声明变量、整脚本按 pass（漏封）
        let out =
            transpile("ipAddress = peer.peerAddress.address\nstrIp = toString(ipAddress)\nstrIp")
                .unwrap();
        assert_eq!(
            out,
            "let ipAddress = peer.peerAddress.address\nlet strIp = (ipAddress).to_string()\nstrIp"
        );
    }

    #[test]
    fn comparison_eq_is_not_treated_as_assignment() {
        let out = transpile("return x == 1;").unwrap();
        assert_eq!(out, "return x == 1;");
    }

    #[test]
    fn transpile_nil_to_unit() {
        let out = transpile("if (x == nil) { return 1; } return 0;").unwrap();
        assert_eq!(out, "if (x == ()) { return 1; } return 0;");
    }

    // ---------- 翻译器：内建函数 ----------

    #[test]
    fn transpile_string_builtins_to_methods() {
        assert_eq!(
            transpile("string.startsWith(peerIdLowercase, \"-gp\")").unwrap(),
            "(peerIdLowercase).starts_with(\"-gp\")"
        );
        assert_eq!(
            transpile("string.indexOf(client, \"gopeed dev\") != -1").unwrap(),
            "(client).index_of(\"gopeed dev\") != -1"
        );
        assert_eq!(
            transpile("string.length('abc')").unwrap(),
            "(\"abc\").len()"
        );
        // Java substring(begin,end) 左闭右开 → rhai sub_string(start, end-begin)
        assert_eq!(
            transpile("string.substring(s, 1, 3)").unwrap(),
            "(s).sub_string(1, (3)-(1))"
        );
    }

    #[test]
    fn transpile_unsupported_builtins_fail_explicitly() {
        assert!(transpile("string.split(s, ',')").is_err());
        assert!(transpile("seq.size(m)").is_err());
    }

    // ---------- seq.map 保序（这是伪装判定的正确性关键） ----------

    #[test]
    fn seq_map_preserves_insertion_order_unlike_rhai_map() {
        // rhai 的 `#{}` map 按键排序；`aria2explorer` 必须先于 `aria2` 被 keys 迭代
        let keys: Vec<String> =
            eval_array("return seq.keys(seq.map('aria2explorer','-ae','aria2','a2'));")
                .iter()
                .map(|d| d.clone().into_string().unwrap())
                .collect();
        assert_eq!(keys, vec!["aria2explorer", "aria2"]);
    }

    #[test]
    fn seq_get_returns_value_in_order() {
        let v = eval_string("let t = seq.map('a','1','b','2'); return seq.get(t, 'b');");
        assert_eq!(v, "2");
    }

    #[test]
    fn seq_list_becomes_array() {
        assert_eq!(eval_array("return seq.list('x','y');").len(), 2);
    }

    // ---------- rhai 语义实证（翻译器的目标行为） ----------

    #[test]
    fn rhai_int_division_is_truncating_like_aviator() {
        let env = build_script_env();
        let v: i64 = env.engine.eval("3/2").unwrap();
        assert_eq!(v, 1, "Aviator 与 rhai 的整数除法都应截断");
    }

    #[test]
    fn rhai_index_of_returns_minus_one_when_missing() {
        let env = build_script_env();
        let hit: i64 = env
            .engine
            .eval("\"hello gopeed dev\".index_of(\"gopeed dev\")")
            .unwrap();
        let miss: i64 = env.engine.eval("\"abc\".index_of(\"zzz\")").unwrap();
        assert_eq!(hit, 6);
        assert_eq!(miss, -1, "index_of 未命中应为 -1（对齐 Java indexOf）");
    }

    #[test]
    fn is_blank_matches_aviator() {
        let env = build_script_env();
        assert!(env.engine.eval::<bool>("is_blank(\"\")").unwrap());
        assert!(env.engine.eval::<bool>("is_blank(\"  \")").unwrap());
        assert!(!env.engine.eval::<bool>("is_blank(\"x\")").unwrap());
        assert!(env.engine.eval::<bool>("is_not_blank(\"x\")").unwrap());
        assert!(!env.engine.eval::<bool>("is_not_blank(\"\")").unwrap());
    }

    #[test]
    fn is_blank_treats_null_as_blank() {
        let env = build_script_env();
        // 上游对 null 实参会抛异常（脚本按 pass 兜底）；此处等价视为空白
        assert!(env.engine.eval::<bool>("is_blank(())").unwrap());
    }

    #[test]
    fn method_form_of_to_lower_and_to_string() {
        let env = build_script_env();
        assert_eq!(
            env.engine.eval::<String>("(\"AbC\").to_lower()").unwrap(),
            "abc"
        );
        assert_eq!(
            env.engine.eval::<String>("(123).to_string()").unwrap(),
            "123"
        );
    }

    #[test]
    fn string_plus_number_concatenates_like_aviator() {
        // 上游 `return '下载=' + downloaded + ' 比例=' + ratio` 依赖跨类型 `+`
        let v = eval_string("return '下载=' + 1024 + ' 比例=' + 0.5;");
        assert_eq!(v, "下载=1024 比例=0.5");
        let v = eval_string("return 7 + '天';");
        assert_eq!(v, "7天");
        // 字符串 + 字符串仍走 rhai 内建
        let v = eval_string("return 'a' + 'b';");
        assert_eq!(v, "ab");
    }

    #[test]
    fn aviator_type_conversion_builtins() {
        // 对齐上游 upload_ratio_check.av 的 `double(uploaded) / double(downloaded)`；
        // `toString(3.0)` 与 Java `String.valueOf(3.0)` 一致输出 "3.0"
        let v = eval_string("return toString(double('1.5') * 2);");
        assert_eq!(v, "3.0");
        // long 对浮点向零截断（对齐 Java (long) cast）
        let env = build_script_env();
        let src = transpile("return long(2.9);").unwrap();
        assert_eq!(env.engine.eval::<i64>(&src).unwrap(), 2);
        // 非法输入抛错 → 脚本按 pass 兜底（对齐上游 NumberFormatException）
        assert!(env
            .engine
            .eval::<i64>(&transpile("return long('abc');").unwrap())
            .is_err());
    }

    // ---------- 端到端：翻译后的社区脚本骨架在引擎里跑通 ----------

    #[test]
    fn end_to_end_masquerade_check_like_name_id_verify() {
        // 复刻 name-id-verify.av 的核心结构（map + for-in + startsWith + 字符串拼接）
        let script = r#"
let table = seq.map(
  'aria2explorer', '-ae',
  'bitcomet', '-bc',
  'aria2', 'a2'
);
for tableName in seq.keys(table) {
  if(string.startsWith(clientNameLowercase, tableName)){
      if(string.startsWith(peerIdLowercase, seq.get(table, tableName))){
          return false;
      }else{
          return 'Peer reporting: PeerId='+peerIdLowercase+', ClientName='+clientNameLowercase + ', But PBH excepted='+ seq.get(table, tableName);
      }
  }
}
return false;
"#;
        let src = transpile(script).unwrap();
        let env = build_script_env();
        let run = |client: &str, pid: &str| -> rhai::Dynamic {
            let mut scope = rhai::Scope::new();
            scope.push("clientNameLowercase", client.to_string());
            scope.push("peerIdLowercase", pid.to_string());
            env.engine
                .eval_ast_with_scope::<rhai::Dynamic>(
                    &mut scope,
                    &env.engine.compile(&src).unwrap(),
                )
                .unwrap()
        };
        // 表内匹配 → false（放行）
        assert!(run("bitcomet 2.0", "-bc0001-x").is_bool());
        // 伪装 → 命中消息（插入序保证 'aria2explorer' 先被匹配）
        let masquerade = run("aria2explorer 1.0", "-rpc27-x");
        assert_eq!(
            masquerade.into_string().unwrap(),
            "Peer reporting: PeerId=-rpc27-x, ClientName=aria2explorer 1.0, But PBH excepted=-ae"
        );
        // 不在表内 → false
        assert!(run("xunlei 3.0", "-xl0001-x").is_bool());
    }
}
