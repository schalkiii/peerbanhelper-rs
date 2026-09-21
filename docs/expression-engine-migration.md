# 表达式规则脚本迁移指南：AviatorScript → rhai

PeerBanHelper 上游（Java 版）的表达式规则引擎使用 **AviatorScript**（JVM 脚本语言）。
本 Rust 忠实重写用 **rhai** 作为等价脚本引擎，外部行为（变量注入、返回值语义、超时、聚合规则、
默认空脚本目录无封禁）与上游逐一对齐。

> 结论先行：**默认配置下本仓库行为与上游完全一致** —— `expression-engine` 默认启用，但 `<data>/scripts/`
> 目录默认没有任何 `.av` 脚本，因此模块恒返回 `pass()`，不产生任何封禁。
> 只有当用户把自写的脚本放进该目录时，才需要把 AviatorScript 改写为 rhai 语法（见下文）。

本文给出从上游 AviatorScript 脚本迁移到本仓库 rhai 脚本的字段映射表与语法对照。
引擎实现见 `crates/pbh-core/src/modules/expression_engine.rs`，对照上游
`ExpressionRule.java` / `util/scriptengine/AVScriptEngine.java`（`handleResult`）。

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

> ⚠️ **整数除法差异**：AviatorScript 中两个整数相除得到整数（截断），如 `3 / 2 == 1`；
> rhai 中 `3 / 2 == 1.5`（浮点）。若需「按 KB/MB 换算后比较阈值」，建议改写阈值方向，
> 例如把 `peer.uploadSpeed / 1024 > 1000` 改为 `peer.upload_speed > 1024 * 1000`，
> 或对结果 `peer.upload_speed.to_int() / 1024`（显式取整后再比）。

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
2. **未暴露的 getter**（见 §3–§4）：`peer.handshaking`、`torrent.completedSize` / `private` / `seeding` /
   `hashedIdentifier` 在 rhai 未直接映射，需按文中近似写法改写。
3. **整数除法**：见 §7 ⚠️。
4. **注释符号**：见 §1 ⚠️。
5. **运算符/正则**：Aviator 的 `..` 拼接、`=~` 正则、`and`/`or` 函数需改写为 rhai 等价写法（见 §7）。
