# 表达式规则脚本迁移指南：AviatorScript → rhai

PeerBanHelper 上游（Java 版）的表达式规则引擎使用 **AviatorScript**（JVM 脚本语言）。
本 Rust 忠实重写用 **rhai** 作为等价脚本引擎，外部行为（变量注入、返回值语义、超时、聚合规则、
默认空脚本目录无封禁）与上游逐一对齐。

> **2026-09-23 起无需手动改写脚本**：引擎加载 `.av` 时会先经 `crates/pbh-core/src/avscript.rs`
> 的 `transpile` 把 AviatorScript **自动翻译**为 rhai，上游社区脚本（PBH-BTN 规则）可原样放入
> `<data>/scripts/` 直接生效。已实测兼容的构造见文末 §10；翻译不了的构造会记日志并跳过该脚本
> （对齐上游「编译失败 → 跳过」）。
>
> 实机社区脚本（`gopeed-random-peerid.av`、`name-id-verify.av`、`2e0-61ff-fe.av`、
> `dot-1-ipv6-tr296.av`）已作为黄金测试固化在 `crates/pbh-core/tests/fixtures/scripts/`，
> 判定结果与 reason 原文由 `tests/av_script_golden.rs` 锁定。
>
> 结论先行：**默认配置下本仓库行为与上游完全一致** —— `expression-engine` 默认启用，但 `<data>/scripts/`
> 目录默认没有任何 `.av` 脚本，因此模块恒返回 `pass()`，不产生任何封禁。
> 只有当用户把自写的脚本放进该目录时，才涉及脚本语言问题（现已自动翻译，见上）。

本文给出从上游 AviatorScript 脚本迁移到本仓库 rhai 脚本的字段映射表与语法对照。
引擎实现见 `crates/pbh-core/src/modules/expression_engine.rs` 与 `crates/pbh-core/src/avscript.rs`，
对照上游 `ExpressionRule.java` / `util/scriptengine/AVScriptEngine.java`（`handleResult`）。

---

## 1. 脚本文件与元数据头

| 项 | AviatorScript（上游） | rhai（本仓库） |
| --- | --- | --- |
| 文件扩展名 | `.av` | `.av`（沿用同一类型，便于直接复用文件名） |
| 脚本目录 | `<data>/scripts/` | `<data>/scripts/`（`PBH_DATA_DIR/scripts` 优先，回退 `data/scripts`） |
| 单条脚本超时 | `maxScriptExecuteTime = 1500ms` | 同 1500ms（超时按 `pass()` 处理） |
| 元数据头 | `# @NAME` / `@AUTHOR` / `@CACHEABLE` / `@VERSION` / `@THREADSAFE` | 同名头；**仅 `@NAME` 用作展示名**，`@CACHEABLE` 被解析但当前不影响行为，其余忽略 |

> ⚠️ **注释写法差异**：rhai 只把以 `# @NAME` / `# @AUTHOR` / `# @CACHEABLE` / `# @VERSION` /
> `# @THREADSAFE` 开头的**元数据行**在编译前剥离；其余 `#` 开头的普通注释**不会被识别为注释**，
> 会导致 rhai 编译失败。请把普通注释写成 `// ...`（rhai 的行注释）。
> 即：AviatorScript 里的 `# 这是注释` 必须改为 `// 这是注释`。

---

## 2. 注入脚本的变量（两者一致）

脚本执行时，引擎会注入以下只读变量（上游 `ExpressionRule.runExpression` 的 `env.put(...)`）：

| 变量 | 类型 | 说明 |
| --- | --- | --- |
| `peer` | Peer | 当前被判定 peer（见 §3） |
| `torrent` | Torrent | 当前种子（见 §4） |
| `downloader` | Downloader | 所属下载器（见 §5） |
| `banDuration` | 整数（bytes? 毫秒） | 配置里的 `ban-duration`（毫秒） |
| `cacheable` | 布尔 | 脚本是否可缓存结果（上游 `AtomicBoolean`） |
| `ramStorage` | Map | 跨脚本/跨调用的线程安全共享存储（上游 `SharedObject.SCRIPT_THREAD_SAFE_MAP`） |
| `moduleInstance` | 字符串 | 固定为 `"expression-engine"` |
| `server` | 布尔 | 固定为 `true` |

---

## 3. `peer` 字段映射（上游 `Peer` 接口 → rhai `PeerData`）

AviatorScript 通过 JavaBean getter 访问属性（如 `peer.getClientName()` → `peer.clientName`）。
rhai 用 snake_case 字段（由 `engine.register_get` 显式暴露）：

> 注：自 2026-09-23 起引擎**同时注册了上游驼峰名**（`peer.clientName` / `peer.peerId` /
> `peer.downloadSpeed` / `peer.uploadSpeed` / `peer.peerAddress`），上游脚本写法可直接使用，
> 下表的 snake_case 列仅作为等价写法参考。

| AviatorScript 写法（上游） | rhai 写法（本仓库） | 类型 | 备注 |
| --- | --- | --- | --- |
| `peer.peerAddress.ip` | `peer.ip` | 字符串 | 上游经 `getPeerAddress().getIp()`；本仓库直接暴露 `ip` |
| `peer.peerAddress.port` | `peer.port` | 整数 | 同上 |
| `peer.peerId` | `peer.peer_id` | 字符串 | `null` 时为空串 |
| `peer.clientName` | `peer.client_name` | 字符串 | `null` 时为空串 |
| `peer.downloadSpeed` | `peer.download_speed` | 整数 (bytes/s) | |
| `peer.uploadSpeed` | `peer.upload_speed` | 整数 (bytes/s) | |
| `peer.downloaded` | `peer.downloaded` | 整数 (bytes) | |
| `peer.uploaded` | `peer.uploaded` | 整数 (bytes) | |
| `peer.progress` | `peer.progress` | 浮点 (0.0–1.0) | |
| `peer.flags` | `peer.flags` | 字符串 | 上游为 `PeerFlag` 对象；本仓库为字符串（与封禁日志一致） |
| `peer.handshaking` | — | — | **未暴露**（如需判断「是否就绪」，可用 `peer.upload_speed <= 0 && peer.download_speed <= 0` 近似） |

---

## 4. `torrent` 字段映射（上游 `Torrent` 接口 → rhai `TorrentData`）

| AviatorScript 写法（上游） | rhai 写法（本仓库） | 类型 | 备注 |
| --- | --- | --- | --- |
| `torrent.id` | `torrent.id` | 字符串 | |
| `torrent.name` | `torrent.name` | 字符串 | |
| `torrent.hash` | `torrent.hash` | 字符串 | |
| `torrent.progress` | `torrent.progress` | 浮点 (0.0–1.0) | |
| `torrent.size` | `torrent.size` | 整数 (bytes) | 即 `total_size` |
| `torrent.rtUploadSpeed` | `torrent.rt_upload_speed` | 整数 (bytes/s) | |
| `torrent.rtDownloadSpeed` | `torrent.rt_download_speed` | 整数 (bytes/s) | |
| `torrent.completedSize` | — | — | **未直接暴露**；用 `torrent.size * torrent.progress` 近似 |
| `torrent.private` (`isPrivate`) | — | — | 未暴露 |
| `torrent.seeding` (`isSeeding`) | — | — | 未暴露（用 `torrent.progress >= 1.0` 近似） |
| `torrent.hashedIdentifier` | — | — | 未暴露 |

---

## 5. `downloader` 字段映射（上游 `Downloader` → rhai `DownloaderInfo`）

| AviatorScript 写法（上游） | rhai 写法（本仓库） | 类型 |
| --- | --- | --- |
| `downloader.id` | `downloader.id` | 字符串 |
| `downloader.name` | `downloader.name` | 字符串 |

---

## 6. 返回值语义（两者完全一致）

上游 `AVScriptEngine.handleResult` 与 rhai `ExpressionEngine::handle_return` 等价：

| 返回类型 | 值 | 结果 |
| --- | --- | --- |
| 布尔 `true` | — | **BAN** |
| 布尔 `false` | — | `pass()` |
| 整数 `0` | — | `pass()` |
| 整数 `1` | — | **BAN** |
| 整数 `2` | — | **SKIP**（跳过，不封不禁；reason 为 `"2"`） |
| 其它整数 | — | `pass()` |
| 字符串（空白） | `""` / `"   "` | `pass()` |
| 字符串（以 `@` 开头） | `"@reason-key"` | **SKIP**，reason 取 `@` 之后的原文（作为 i18n key） |
| 字符串（其它） | `"some text"` | **BAN**，reason 为字符串本身 |
| （上游）`PeerAction` / `CheckResult` | — | 上游会直接使用；**rhai 实现将其视作 `pass()`**（实践中脚本多返回 bool/number/string，无行为差异） |
| 抛异常 / 超时 | — | `pass()`（上游 `runExpression` 兜底） |

> 多条脚本聚合（上游 `shouldBanPeer`）：**SKIP 优先级最高（短路返回）**，其次 BAN，否则 `pass()`。
> 与上游一致。

---

## 7. 语法对照（常见迁移点）

| 概念 | AviatorScript（上游） | rhai（本仓库） | 说明 |
| --- | --- | --- | --- |
| 行注释 | `# ...` | `// ...`（仅 `# @NAME` 等元数据头例外） | 见 §1 ⚠️ |
| 块注释 | `/* ... */` | `/* ... */` | |
| 变量声明 | `let x = 1;` | `let x = 1;` | 相同 |
| 布尔 | `true` / `false` | `true` / `false` | 相同 |
| 字符串字面量 | `"..."` | `"..."` | 相同 |
| 空值 | `nil` | `()` | |
| 逻辑与/或 | `&&` / `\|\|`（或 `and` / `or`） | `&&` / `\|\|` | Aviator 的 `and`/`or` 函数 → 改用 `&&`/`\|\|` |
| 比较 | `==` `!=` `>` `<` `>=` `<=` | 同 | |
| 算术 | `+` `-` `*` `/` `%` | `+` `-` `*` `/` `%` | ⚠️ 见下 |
| 三元 | `a ? b : c` | `if a { b } else { c }` | |
| 字符串拼接 | `s1 .. s2` 或 `s1 + s2` | `s1 + s2` | `..` 在 rhai 无效 → 用 `+` |
| 正则匹配 | `s =~ /re/` | 用字符串方法或 `regex` 包 | rhai 无 `=~` 运算符 |
| 方法调用 | `obj.method()` / bean getter | `obj.method()`（snake_case） | 见 §3–§5 |
| `return` | `return expr;` | `return expr;` | 相同；脚本末行表达式即返回值 |
| 循环 | `for ... in` / `while` | `for ... in` / `while` / `loop` | |
| Map 读写 | `m.key` / `m["key"]` | `m.key` / `m["key"]` | `ramStorage` 为 rhai Map |

> ✅ **整数除法**：AviatorScript 与 rhai 的两个整数相除**都得到截断整数**（`3 / 2 == 1`，
> 已在单测 `avscript::tests::rhai_int_division_is_truncating_like_aviator` 中锁定）。
> 此前文档误记 rhai 为浮点除法，特此更正——除法语义无需改写。

---

## 8. 迁移示例

### 示例 1：按客户端名前缀封禁

上游 AviatorScript：

```aviatorscript
# @NAME Ban qBittorrent 4.x
let client = peer.clientName;
if string.startsWith(client, "qBittorrent/4.") {
  return true;
}
return false;
```


本仓库 rhai（`.av`）：

```rhai
// @NAME Ban qBittorrent 4.x
let client = peer.client_name;
if client.starts_with("qBittorrent/4.") {
  return true;
}
return false;
```

（`return peer.client_name.starts_with("qBittorrent/4.");` 一行即可）

### 示例 2：按上传速度阈值封禁（注意除法差异）

上游 AviatorScript（整数除法）：

```aviatorscript
let kb = peer.uploadSpeed / 1024;
if kb > 1000 {
  return 1;
}
return 0;
```


本仓库 rhai（改写阈值方向，避免浮点截断歧义）：

```rhai
if peer.upload_speed > 1024 * 1000 {
  return 1;
}
return 0;
```


### 示例 3：字符串返回携带 reason / SKIP

上游 AviatorScript：

```aviatorscript
return "Too many connections";   // BAN，reason = "Too many connections"
return "@user-defined-skip";     // SKIP，reason key = "user-defined-skip"
```


本仓库 rhai（完全一致）：

```rhai
return "Too many connections";
return "@user-defined-skip";
```


### 示例 4：用种子进度近似 completedSize

上游 AviatorScript：`let done = torrent.completedSize;`
本仓库 rhai（字段未直接暴露）：

```rhai
let done = torrent.size * torrent.progress;
```


### 示例 5：多脚本聚合（SKIP 优先）

上游 AviatorScript 写两条脚本：一条 `return 1;`（BAN），一条 `return 2;`（SKIP）。
本仓库 rhai 同样：引擎按「SKIP 优先于 BAN、否则 pass」聚合，与上游短路语义一致。

---

## 9. 已知细微差异（默认配置下无行为影响）

1. **直接返回 `PeerAction` / `CheckResult` 对象**：上游 `handleResult` 会直接使用；rhai 实现将其视为 `pass()`。
   实践中用户脚本均返回 bool / 数字 / 字符串，故无实际差异。
2. **`peer.handshaking` 未暴露**（`PeerData` 无对应字段）；其余 §3–§4 标注「未暴露」的字段
   （`torrent.completedSize` / `private` / `seeding` / `hashedIdentifier`）自 2026-09-23 起已按上游
   `Torrent` 接口语义补齐，可直接使用。
3. **整数除法**：见 §7 ✅（两语言一致，无差异）。
4. **`isBlank(nil)`**：上游对 null 实参抛异常 → 整脚本按 pass 兜底；本移植把 null 字段暴露为空串、
   `isBlank("")` 为 true —— 对使用 `if(isBlank(x)) return false;` 守卫的社区脚本，最终判定结果一致。
5. **运算符/正则**：Aviator 的 `=~` 正则、`and`/`or` 函数暂不支持（编译报错跳过，见 §10）。

---

## 10. AviatorScript → rhai 自动翻译兼容清单（`avscript::transpile`）

加载 `.av` 时自动应用；以下构造**无需改写**：

| 构造 | 翻译结果 | 备注 |
| --- | --- | --- |
| `##` 行注释（含 `## @NAME` 元数据头） | `//` | 元数据 `@NAME` 仍解析为展示名 |
| 单引号字符串 `'…'` | 双引号 `"…"`（转义改写） | |
| `isBlank(x)` / `isNotBlank(x)` | `is_blank(x)` / `is_not_blank(x)` | 注册的自定义函数；null 视为空白 |
| `toLowerCase(x)` / `toUpperCase(x)` / `toString(x)` | `(x).to_lower()` 等 | |
| `string.contains/startsWith/endsWith/indexOf(s,…)` | `(s).contains(...)` 等 | `indexOf` 未命中返回 `-1`，与 Java 一致 |
| `string.length(s)` / `string.trim(s)` | `(s).len()` / `(s).trim()` | `len` 按 Unicode 字符计（Java 为 UTF-16 码元） |
| `string.substring(s, b[, e])` | `(s).sub_string(b[, e-b])` | Java 左闭右开 → rhai 长度语义 |
| `seq.map(k1,v1,…)` | 扁平数组 `[…]` | **保插入序**（rhai 的 `#{}` 按键排序会改变 `name-id-verify` 的匹配优先级） |
| `seq.keys(m)` / `seq.get(m,k)` | `av_keys(m)` / `av_get(m,k)` | 保序访问函数 |
| `seq.list(a,b,…)` | 数组字面量 `[a,b,…]` | |
| 语句级裸赋值 `x = …` | `let x = …` | rhai 允许 `let` 重声明遮蔽 |
| `nil` | `()` | |
| 属性访问 `peer.clientName` / `peer.peerAddress.address` / `torrent.completedSize` 等 | 原样 | 引擎同时注册驼峰 + snake_case getter，`peerAddress.address` 为规范化地址串 |

**暂不支持**（显式报错跳过，日志注明原因）：`string.split` / `string.join` / `string.replace_first` /
`string.replace_all`、`seq.size`、三目 `?:`、正则 `=~`、`math.*`、`for (x : list)` 旧循环语法。
如需这些构造，请把脚本改写为 rhai 等价写法（本文 §7 的对照表仍然适用）。
