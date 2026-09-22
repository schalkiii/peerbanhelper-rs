# SPEC — PeerBanHelper-RS 功能契约（忠实重写）

本文件定义 Rust 重写必须满足的行为契约，基线为上游 Java 版 v9.5.1。标注 **[GOLDEN]** 的行为必须有黄金测试覆盖。
所有判定阈值、默认值、过滤条件、API 载荷均以本文件为准，实现时不得擅自更改。

---

## 1. 运行模型

- 程序以固定周期执行一轮 **ban wave**，周期由 `profile.yml` 的 `check-interval`（毫秒）配置，**默认 5000ms**。
- 每轮流程（对应上游 banpipeline 的 organ 流水线）：
  1. `DownloaderLoginOrgan`：对每个下载器执行登录/会话校验。
  2. `TorrentsFetchOrgan`：拉取活跃 torrent 列表。
  3. `PeersFetchOrgan`：逐 torrent 拉取 peers；每个下载器用信号量限制并发请求
     （qB 默认最大并发槽 **128**）。
  4. `RunCheckModuleOrgan`：对每个 peer **执行所有启用的规则模块**（不短路），产出多个 `CheckResult`。
  5. `UpdateSnapshotOrgan`：更新内存快照与持久化。
  6. 汇总封禁集合，调用下载器封禁 API（增量优先，必要时全量）。
- **多模块结果聚合 [GOLDEN]**（`DigestionSession.extractFromLastOrgan`）：同一 peer 的多个
  `CheckResult` 按下列优先级合并，**不是**「首个命中模块即短路」：
  1. `PeerAction` 等级更高者胜：`NO_ACTION(0) < BAN_FOR_DISCONNECT(1) < BAN(2) < SKIP(3)`；
  2. 等级相同时取 **更长** 的 `banDuration`（避免封禁时长被缩短）；
  3. 完全并列时，上游因并发执行而不确定；Rust 取**模块注册顺序更早者**（确定性实现）。
- bypass（命中 `ignore-peers-from-addresses`）在模块执行前直接产出 `SKIP`，不再执行任何规则模块。
- 单个 torrent 的 peers 拉取/判定有超时，超时不影响其他 torrent（隔离失败）。

## 2. 领域模型

### 2.1 PeerAddress

- 字段：`ip`（用于判定的 IP）、`port`、`downloader_raw_ip`（下载器返回的原始 `ip:port` 键）。
- 构造：qB 中 `rawIp` 为 `sync/torrentPeers` 返回的 peers Map 的键（`ip:port` 或 `[ipv6]:port`）。
- NAT/Teredo 转换为后续阶段能力（本阶段保留字段，不启用转换）。

### 2.2 PeerFlag（libtorrent flags）**[GOLDEN]**

解析 qB `flags` 字符串（字符集合），与上游 `PeerFlag.parseLibTorrent` 完全一致：

| 字符 | 含义 |
| --- | --- |
| `d` / `D` | interesting=true；remoteChoked=true / false |
| `u` / `U` | remoteInterested=true；choked=true / false |
| `K` | remoteChoked=false, interesting=false |
| `?` | choked=false, remoteInterested=false |
| `O` | optimisticUnchoke |
| `S` | snubbed |
| `I` | 非本地连接（localConnection=false） |
| `H` / `X` / `L` | 来自 DHT / PEX / LSD |
| `E` / `e` | RC4 加密 / 明文加密 |
| `P` | uTP 套接字 |

### 2.3 Peer

| 字段 | qB JSON 键 |
| --- | --- |
| clientName | `client` |
| peerId | `peer_id_client` |
| downloadSpeed | `dl_speed` |
| downloaded | `downloaded` |
| uploadSpeed | `up_speed` |
| uploaded | `uploaded` |
| progress | `progress`（0.0–1.0+） |
| flags | `flags`（按 2.2 解析） |
| ip / port | `ip` / `port` |

- **握手判定 [GOLDEN]**：`up_speed <= 0 && dl_speed <= 0` 即视为握手中（`isHandshaking`）。
- 缓存键：`ip:port`。

### 2.4 Torrent

| 字段 | qB JSON 键 |
| --- | --- |
| id / hash | `hash` |
| name | `name` |
| progress | `progress` |
| size | `total_size` |
| completedSize | `piece_size>0 && pieces_have>0 ? piece_size*pieces_have : -1` |
| rtUploadSpeed / rtDownloadSpeed | `upspeed` / `dlspeed` |
| private | `is_private`（可空；旧版本缺失时由 properties API 补全） |
| seeding | `progress >= 1.0` |

## 3. qBittorrent 适配器契约 **[GOLDEN]**

端点前缀：`{endpoint}/api/v2`。

### 3.1 登录与会话

- **会话复用优先 [GOLDEN]**：先 `GET /app/buildInfo` 探测会话（HTTP 200 且 `libtorrent` 非空）；
  若仍有效则直接成功返回，**不再** `POST /auth/login`（否则每轮 wave 都会多打一次登录请求）。
- 会话失效时 `POST /auth/login`，表单 `username`、`password`；成功依赖 Cookie 会话（qB 返回 `SID`）。
  登录后必须再次通过 buildInfo 校验，才算成功。
- API Key 认证：探测失败时**不**尝试表单登录，直接返回凭据错误。
- 版本：`GET /app/version`，去掉前导 `v`，按 LOOSE 语义解析；
  口令认证要求 **>= 4.5.0**，API Key 认证要求 **>= 5.2.0**。
- 首次健康时调用 `POST /app/setPreferences`，表单 `json={"enable_multi_connections_from_same_ip":false}`
  （可用开关 `pbh.downloader.qbittorrent.disableSameIpMultiConnection`，默认 true）。
- **[GOLDEN] HTTP Basic Auth**：**不预置**凭据，收到 **401** 后才带 Basic 重试一次
  （对齐上游 OkHttp `Authenticator` 的 `responseCount <= 1`）。
- API Key（qB >=5.2.0）：对除 `/auth/login`、`/auth/logout` 外的请求加 `Authorization: Bearer <key>`。
- 未登录时 `GET /app/buildInfo` 返回 403，视为“会话无效”。

### 3.2 拉取 torrent 列表

- `GET /torrents/info?filter=active&limit=100&offset={n}`（活跃检查只拉 active；全量不带 filter）。
- 分页：页大小 100；按 `hash` 去重；当一页无新增或返回数 < 100 时停止。
- `ignorePrivate=true`（默认）时过滤私有种子。补全规则 **[GOLDEN]**：
  - `ignorePrivate` 且 `is_private` 缺失 → 用 properties 的 `is_private` 补全；
  - `piece_size <= 0` **或** `pieces_have <= 0` → 两者都用 properties 的值覆盖；
  - properties 走 `GET /torrents/properties?hash=`，带缓存，TTL = check-interval + 60s（默认 65s）。

### 3.3 拉取 peers

- `GET /sync/torrentPeers?hash={hash}`，响应形如 `{"full_update":..,"peers":{"ip:port":{...}}}`。
- 遍历 peers Map，**跳过**：
  - `connection` 为 `HTTP`/`HTTPS`/`Web`（大小写不敏感）；
  - `ip` 为空；
  - 键名包含 `.onion` 或 `.i2p`。
- 其余构造 Peer，`rawIp` = Map 键。

### 3.4 封禁 **[GOLDEN]**

封禁状态保存在**内存封禁表**中（键为 IP），每轮 wave 的顺序与上游 `banWave()` 一致：

1. `removeExpiredBans()`：`now > unbanAt`（**严格大于**）的条目解封，并从 `banned_ips` 表删除；
2. 判定流水线产出的 `BAN` / `BAN_FOR_DISCONNECT` 逐条写入（重复封禁同一地址会标记
   `needReApplyBanList`，下一轮强制全量重放，且以最后一次的时长为准）；
3. 下发：

- **增量**：`removed` 为空 **且** `increment-ban=true` **且** 非全量重放 →
  `POST /transfer/banPeers`，表单 `peers={rawIp}|{rawIp}`（`|` 连接，rawIp 为 `ip:port`）。
  **[GOLDEN] 载荷是本轮「跨下载器全局新增」集合**：上游把 `bannedPeers`（本轮所有下载器新增的封禁）
  传给每个下载器，因此任一下载器新增的封禁项也会下发给其它下载器；
  只要本轮任一新增或解封存在，所有下载器都会被通知。
- **全量**：其余情况（有任何解封项 / 关闭增量 / 需要全量重放）→
  `POST /app/setPreferences`，表单 `json={"banned_IPs":"ip1\nip2\n..."}`（`\n` 连接）。
  全量载荷取自**内存封禁表**（已解封的条目自然消失），不是数据库全表。
- 本轮既无新增也无解封时，**不发起任何请求**。

#### banlist-remapping（CIDR 重映射）**[GOLDEN]**

对齐 `IPAddressUtil.remapBanListAddress(address, supportRangeBan)`：

- `banlist-remapping.ipv4`（默认 `enabled: false`、`remap-range: 30`）；
  `banlist-remapping.ipv6`（默认 `enabled: true`、`remap-range: 52`）。
- `supportRangeBan` 取决于下载器是否声明 `RANGE_BAN_IP` 能力（qB 需 >= 5.3.0 / 5.2.0-beta1）；
  为 false 时**只下发单个地址**，不生成任何网段。
- 除单地址外还会追加等价写法：NAT64 → 内嵌 IPv4；Teredo → 内嵌客户端 IPv4；
  IPv6 地址若命中 NAT64/Teredo 则不参与 `/52` 重映射。
- 输出格式：上游为 inet.ipaddr `toCompressedString()`，本实现为 RFC 5952（Rust `Ipv6Addr`）。
  两者**实际封禁集合等价**；上游额外为 IPv4 生成的 IPv4-mapped 写法（`::ffff:a.b.c.d`）
  对下载器不命中真实 IPv4 peer，属空操作，本实现不生成。

### 3.4.1 Transmission 适配器 **[GOLDEN]**

上游 `Transmission`（要求 **>= 4.1.0**，已明确不支持 4.1.0 以下版本）：

- RPC：`{endpoint}{rpc-url}`（默认 `/transmission/rpc`），JSON 请求体
  `{"method": ..., "arguments": {...}}`；首次请求返回 **409**，需从
  `X-Transmission-Session-Id` 头取会话 id 并重试一次（对齐 cordelia `TrClient`）。
- 登录：`session-get` → 版本截断到 **5 个字符** → 要求 `>= 4.1.0`；
  随后确保 blocklist 指向 PBH 自身端点：`!blocklist-enabled || !blocklist-url.startsWith(pbhUrl)`
  时执行 `session-set{blocklist-url: <pbhUrl>?t=<now>, blocklist-enabled: true}` + `blocklist-update`；
  `blocklist-update` 失败时把 URL 写成 `http://peerbanhelper-blocklist-update-failed.com/...`
  并登录失败（便于用户在 WebUI 发现）。
- 拉取种子：`torrent-get`，字段 `id/hashString/name/peersConnected/status/totalSize/peers/
  rateDownload/rateUpload/peerLimit/percentDone/sizeWhenDone/trackerList/trackerStats/isPrivate`。
  - 活跃过滤：`rateDownload > 0 || rateUpload > 0 || peersConnected > 0`；
  - `ignore-private`（Transmission 默认 **false**）为真时过滤私有种子。
  - **`completedSize = (long)(sizeWhenDone * percentDone)`**（与 qB 的 `piece_size*pieces_have` 不同）。
- peers：随 `torrent-get` 一起返回并缓存在适配器内，`fetch_peers` 直接读取（不额外发请求）。
  - `peer_id` 是 base64 的 20 字节，解码为 **ISO-8859-1** 文本；缺失时为 `""`；
  - 速度/字节数缺失时按 `-1`（对齐上游 `TRPeer`），`isHandshaking` 仍为两侧速度 `<= 0`。
- 封禁：Transmission **不支持按 peer 增量封禁**，只支持整份 blocklist 更新 →
  `ban_peers` 为空操作，`replace_banned_ips` 触发 `blocklist-update`
  （列表内容由 PBH 的 `/blocklist/p2p-plain-format` 提供）。因此本下载器强制走全量路径。
- 能力标志：`UNBAN_IP` / `TRAFFIC_STATS` / `LIVE_UPDATE_BT_PROTOCOL_PORT`（**无 `RANGE_BAN_IP`**）。
- 统计：`session-stats` → `cumulative-stats.uploadedBytes/downloadedBytes`。

### 3.5 下载器能力标志 **[GOLDEN]**

对齐 `AbstractQbittorrent.getFeatureFlags()`（与 Java `DownloaderFeatureFlag` 同名）：

- 恒定声明：`UNBAN_IP`、`TRAFFIC_STATS`、`LIVE_UPDATE_BT_PROTOCOL_PORT`；
- `RANGE_BAN_IP` 仅在版本满足 `>= 5.3.0`（含 `5.3.0-alpha1`）或等于 `5.2.0-beta1` 时声明；
  未登录（尚无版本信息）时不声明。
- 能力标志只影响全量封禁列表是否做 CIDR 重映射，以及 `UNBAN_IP` 是否参与 PCB 快速测试。

### 3.6 地址翻译（peer 地址）**[GOLDEN]**

对齐 `AbstractDownloader.addressTranslate`（在 `getPeers` 解析阶段执行，顺序不可调换）：

1. 内置 NAT（AutoSTUN）——需要 AutoSTUN 服务端，本阶段不支持；
2. Teredo（`ip-remapping.teredo`，默认 **false**）→ 按 `extractTeredo` 改写 IP **与端口**；
3. NAT64（`ip-remapping.nat64.enabled` 默认 true，`prefix` 默认 `["64:ff9b::/96"]`）
   → 取低 32 位作为内嵌 IPv4，端口不变；
4. IPv4-mapped（`::ffff:a.b.c.d`）归一到 IPv4。

- `raw_ip`（下载器返回的 `ip:port` 键）**不翻译**，增量封禁仍使用原始键。
- 判定、bypass、封禁列表使用翻译后的 IP。

### 3.7 时间outs

连接 10s，读/写 30s；连接池大小 = 并发槽 + 10，keep-alive 5 分钟。

## 4. 规则引擎契约 **[GOLDEN]**

### 4.1 规则 JSON 语法

每条规则是一个 JSON 对象（也兼容布尔/数字/字符串/null 虚拟规则）：

```json
{"method": "CONTAINS", "content": "xxx", "hit": "TRUE", "miss": "DEFAULT"}
```

- `method`：`STARTS_WITH` / `ENDS_WITH` / `CONTAINS` / `EQUALS` / `REGEX` / `LENGTH`。
- `content`：匹配内容；除 `REGEX`、`EQUALS` 外，匹配前规则与输入都转 **小写**（ROOT locale）。
  这是 **Unicode 感知**的 `toLowerCase(Locale.ROOT)`，**不是** ASCII-only 小写。
- `EQUALS`：Java `String.equalsIgnoreCase`（逐字符 `toUpperCase`/`toLowerCase` 比较，Unicode 感知）。
- `LENGTH` 使用 `min`、`max`（含边界，`min <= len <= max` 命中），
  长度口径为 Java `String.length()` 即 **UTF-16 码元数**（增补平面字符按 2 计）。
- `REGEX` 大小写敏感，使用标准正则 `find`（部分匹配即命中，等价 Java `matcher().matches()` 中的 `find` 语义——
  注意上游 `StringRegexMatcher` 用的是 `matcher(content).matches()`，即 **整段匹配 `matches()`**；
  Rust 端用 `^(?:pattern)$` 等价对齐，见 §4.4）。
- `hit`/`miss`：`TRUE` / `FALSE` / `DEFAULT`，缺省 hit=TRUE、miss=DEFAULT。

### 4.2 多规则裁决语义（matchRule）**[GOLDEN]**

按列表顺序遍历：

- 任一规则返回 **FALSE → 立即返回「不命中」**（FALSE 优先级最高，短路）；
- 规则返回 **TRUE → 覆盖记录命中**，但继续遍历（可被后续 FALSE 覆盖）；
- DEFAULT 不改变结果；
- 遍历结束未遇 FALSE 且曾有 TRUE → 命中；否则不命中。

**[GOLDEN] 每次 TRUE 都覆盖上一次记录**，因此最终「命中的规则」是**最后一条**返回 TRUE 的规则
（不是第一条）。该下标/规则会进入封禁日志与 API 展示，必须一致。
规则列表顺序因此是有语义的，默认规则集顺序不得调整。

### 4.3 模块判定流程

对每个 peer，模块返回 `CheckResult{action, duration, rule_name, reason, data}`，action ∈
`BAN` / `BAN_FOR_DISCONNECT` / `SKIP` / `NO_ACTION`。

**握手前置条件 [GOLDEN]**（`isHandShaking == up_speed <= 0 && dl_speed <= 0`）：

- PeerId / ClientName 黑名单：`握手中 && (标识 == null || 标识 isBlank)` → 返回 `handshaking`（NO_ACTION）；
  **仅握手中但已携带标识时仍要参与规则判定**（否则吸血客户端把速度压成 0 即可绕过黑名单）。
- IP 黑名单 / PCB：只要握手中即返回 `handshaking`。

### 4.4 正则语义对齐说明

Java `StringRegexMatcher`：`Pattern.compile(content).matcher(input).matches()`，`matches()` 要求**整段完全匹配**。
Rust `regex` crate 的 `is_match` 是部分匹配，因此封装为 `^(?:content)$` 包裹后 `is_match`，并对 `content`
以 `(?:...)` 处理，保证与 Java 一致。黄金测试覆盖该差异。

## 5. 内置规则模块

### 5.1 PeerId 黑名单 `peer-id-blacklist`

- 默认启用，`ban-duration` 默认 259200000ms（3 天），模块级可覆盖全局。
- 对 `peer_id_client` 应用规则集；默认规则集见上游 `profile.yml`（`-hp`/`-xm`/`-dt`/`-sd` 等前缀，
  `cacao` 包含，`Unknown` 相等等）。**[GOLDEN]**
- **[GOLDEN] 默认规则集必须与上游 `profile.yml` 逐字逐序一致**，共 16 条：
  `-hp`、`-xm`、`-dt`、**`CONTAINS -rn0.0.0`（第 4 条！）**、`-sd`、`-xf`、`-qd`、`-bn`、`-dl`、
  `-ts`、`-fg`、`-tt`、`-nx`、`CONTAINS cacao`、`EQUALS Unknown`、`EQUALS 未知`。

### 5.2 ClientName 黑名单 `client-name-blacklist`

- 默认启用，`ban-duration` 默认 259200000ms。
- 对 `client` 应用规则集；默认规则集见上游 `profile.yml`（`hp/torrent`、`xfplay`、`flashget`、
  `StellarPlayer`、`qbittorrent/3.3.15` 等）。**[GOLDEN]**
- **[GOLDEN] 两条易错点**（默认集会直接改变封禁结果）：
  1. `qbittorrent/3.3.15`、`github.com/thank423/trafficconsume` 与第 19 条特殊规则均为
     **`STARTS_WITH`**（不是 `CONTAINS`）；
  2. 第 19 条的 `content` 是上游 `profile.yml` 第 92 行的真实字节 `\xde\xad__`
     （U+07AD 后跟两个下划线；行内注释 `0xde-0xad-0xbe-0xef` 是误导，**不要**照抄成中文或 4 字节序列）。

### 5.3 IP 黑名单 `ip-address-blocker`

- 支持单 IP、CIDR（IPv4/IPv6）、IP 范围；命中即 BAN。
- 内置忽略地址（bypass，来自 `ignore-peers-from-addresses`）：`10.0.0.0/8`、`172.16.0.0/12`、
  `192.168.0.0/16`、`fc00::/7`、`100.64.0.0/10`、`169.254.0.0/16`、`127.0.0.0/8`、`fe80::/10`。
  来自这些地址的 peer **跳过所有检查**（SKIP）。**[GOLDEN]**

### 5.4 虚假进度检查器 `progress-cheat-blocker` **[GOLDEN]**

默认参数逐字对齐上游 `profile.yml`：

| 参数 | 默认值 |
| --- | --- |
| `minimum-size` | 50,000,000 |
| `maximum-difference` | 0.10 |
| `rewind-maximum-difference` | 0.07 |
| `block-excessive-clients` / `excessive-threshold` | true / 1.5 |
| `ipv4-prefix-length` / `ipv6-prefix-length` | 32 / 56 |
| `ban-duration` | 2,592,000,000（30 天） |
| `max-wait-duration` | 30,000 |
| **`fast-pcb-test-percentage`** | **0.1（默认启用）** |
| **`fast-pcb-test-block-duration`** | **15,000** |

- `minimum-size`：torrent 小于 50,000,000 字节不检查。
- 依据「我们实际传给该 peer 的字节数」推算其**最小应达进度**：
  `min_progress = uploaded_to_peer / torrent.completedSize`（结合 torrent 进度与完成量）。
- `maximum-difference`（默认 0.10）：若 peer 自报进度比推算最小进度低超过 10 个百分点 → BAN。
- 进度倒退：默认允许最多 7% 倒退（超过则 BAN），用于识别进度回退作弊。
- 快速 PCB 测试：下载器具备 `UNBAN_IP` 能力且从未测过该 peer 时，
  一旦实际上传量 ≥ `fast-pcb-test-percentage * torrent_size` →
  返回 `BAN_FOR_DISCONNECT`（`rule = fastPcbTest`，时长 `fast-pcb-test-block-duration`），
  并记录 `fast_pcb_test_executed`，同一 peer 只触发一次。
- 状态实体与缓存键 **[GOLDEN]**：
  - 前缀实体键 `(downloader, torrent, prefix)`，`prefix` 为 IPv4/32、IPv6/56 的 `toPrefixBlock`；
  - IP 实体键 `(downloader, torrent, **ip**)`——**不含端口**。若把端口计入键，
    同一 IP 的多个端口会各自累加上传增量，导致 `computedUploaded` 虚高而误判「过量下载」/进度差。
  - 解封窗口（`ban-delay-window`）到期判定为 `now > window_end`（严格大于）。
- 需要跨轮历史（peer 首次出现、上传量、曾达最大进度），历史持久化到 SQLite。
- **持久化契约 [GOLDEN]**（`enable-persist` 默认 true）：
  - 每轮 wave 结束后把**变更过**（dirty）的实体写回 `pcb_addr` / `pcb_range`；
  - 进程启动时载入未过期记录，因此「已开窗等待」的 peer 重启后不会重新获得宽限期；
  - `pcb_addr` 行的 `port` 取该 IP **首次出现**时的端口（判定本身不使用端口）；
  - 每 8 小时清理超过 `persist-duration` 未出现的记录（对齐 `cleanDatabase`）。

### 5.5 范围自动封禁 `auto-range-ban` **[GOLDEN]**

- 默认启用，`ban-duration` 默认 604800000ms（7 天），`ipv4: 30`、`ipv6: 48`。
- 判定（对齐 `AutoRangeBan.shouldBanPeer`）：
  1. 握手中 → **返回 `pass()`**（注意上游此处不是 `handshaking()`）；
  2. peer 自身已在内存封禁表中 → `pass()`；
  3. 遍历内存封禁表：跳过 `BAN_FOR_DISCONNECT` 记录、跳过地址族不同的记录；
  4. 把被扫描的封禁地址按 `ipv4`/`ipv6` 前缀长度归拢成网段，若包含当前 peer 地址 → `BAN`。
- 文案：`rule` = 字面量地址类型（`IPv4/30` / `IPv6/48`，文案表无此键故原样输出）；
  `reason` = `ARB_BANNED`（模板只有 3 个占位符，第 4 个参数被 `fillArgs` 忽略）；
  结构化数据 `relatedBannedAddress` = 命中的已封禁地址。
- 依赖与 wave 共享的**内存封禁表**（本 wave 内的新封禁不参与本 wave 的连锁判定，
  对齐上游先跑完整个 digestion 再逐个 `banPeer` 的顺序）。

### 5.6 多拨封禁 `multi-dialing-blocker` **[GOLDEN]**

- 默认启用，`ban-duration` 默认 1296000000ms（20 天）。
- 默认参数：`subnet-mask-length: 24`、`subnet-mask-v6-length: 56`、
  `tolerate-num-ipv4: 2`、`tolerate-num-ipv6: 5`、`cache-lifespan: 86400`（秒）、
  `keep-hunting: false`、`keep-hunting-time: 2592000`（秒）。
- 判定（对齐 `MultiDialingBlocker.shouldBanPeer`）：
  1. 握手中 → `handshaking()`；
  2. 记录 `torrentId@ip` 与 `torrentId@subnet`（子网按地址族取对应前缀长度）；
  3. 子网内**不同 IP 数** > 容忍值 → `BAN`（`MDB_MULTI_DIALING_DETECTED` +
     `MODULE_MDB_MULTI_DIALING_DETECTED(subnet, ip)`），并写入追猎名单；
  4. `keep-hunting` 打开且子网在追猎窗口内 → `BAN`（`MDB_MULTI_HUNTING` +
     `MODULE_MDB_MULTI_DIALING_HUNTING_TRIGGERED(subnet, ip)`），命中后刷新窗口；
     超出窗口则移除追猎记录。
- 状态过期语义对齐 guava Cache：peer 记录 `expireAfterWrite = cache-lifespan`、
  子网分组 `expireAfterAccess = cache-lifespan`、追猎名单窗口 = `keep-hunting-time`；
  所有时间取自 `CheckContext.now_ms`（可确定性测试）。
- 说明：追踪 `/48` 下第 4 段为 0 的前缀（如 `2001:db8:1:2::1` 的 IPv6/48 前缀是
  `2001:db8:1::/48`）是本实现的确定性输出，与 `toPrefixBlock` 语义一致。

### 5.7 反吸血 `anti-vampire` **[GOLDEN]**

- 默认启用，`ban-duration` 默认 14400000ms（4 小时），`presets.xunlei.enabled: true`。
- **不做握手判定**（上游直接进入预设检查）。
- 迅雷预设识别：`peer_id` 小写后以 `-xl` 开头（`-xl0019` 记为 0019 版本）；
  `client` 小写后以 `xunlei` 开头（包含 `0019` 或 `0.0.1.9` 记为 0019 版本）。
- 判定：
  - 非迅雷 → `pass()`；
  - 迅雷且非 0019 → `BAN`（`MODULE_ANTI_VAMPIRE_TITLE` +
    `MODULE_ANTI_VAMPIRE_DESCRIPTION_XUNLEI_NON_0019`，data `xunleiType=non-0019`）；
  - 迅雷 0019 且在**做种**任务上 → `BAN`（`…_XUNLEI_0019_SEEDING`，data `xunleiType=0019`）；
  - 迅雷 0019 且下载中 → `pass()`（它正常参与 swarm 分享）。

### 5.8 IP 黑名单规则订阅 `ip-address-blocker-rules` **[GOLDEN]**

- 默认启用，`ban-duration` 默认 259200000ms（3 天），`check-interval` 默认 14400000ms（4 小时；
  代码内兜底默认 86400000），默认订阅 `rules.all-in-one` →
  `https://bcr.pbh-btn.com/combine/all.txt`。
- **规则文本语法**（`rules.<id>.url` 指向的纯文本，逐行）：
  - 空行跳过；以 `#` 或 `//` 开头的整行是**注释行**，累积给下一条 IP 规则
    （多行用 `\n` 连接；注释保留标记后的原文，含前导空格）；
  - 含 `,` 的行按 DAT/eMule 格式 `start , end , level [, comment]`：字段数 < 3 丢弃、
    `level >= 128` 丢弃（上游注释里的示例 `200` 同样被丢弃）、
    区间取「覆盖整个区间的最小前缀块」（`coverWithPrefixBlock`）；字段用 `Integer.parseInt`
    解析且**不 trim**，因此带空格的 DAT 行会被丢弃（保留该行为）；
  - 其余行取**第一个** `#` 或 `//` 之前的地址（支持 CIDR 与前导零写法 `016.0.0.0`），
    之后为尾注释（优先于累积注释）；
  - 同一网段重复出现时合并注释（`\n` 连接）。
- **匹配**：最长前缀命中（对齐 trie 的 `elementsContaining`）；命中即 `BAN`：
  `rule` = 规则名（字面量，文案表通常无此键）、
  `reason` = `MODULE_IBL_MATCH_IP_RULE(规则名, IP, 备注)`，结构化数据 `ruleName`。
  备注缺省时为空字符串（上游 `MODULE_IBL_COMMENT_UNKNOWN` 实际不可达）。
- **刷新**（应用层，对齐 `updateRule`）：
  1. 远程内容缓存到 `<data>/sub/<ruleId>.txt`；
  2. sha256 比对：远程与缓存不同 → 解析远程并写回缓存；相同但内存未加载 → 解析远程（不写盘）；
     相同且已加载 → 跳过；
  3. 远程不可用（含非 2xx，上游未检查状态码，本实现按失败处理）→ 若存在缓存且未加载则用缓存；
  4. 规则被禁用或 URL 为空 → 从模块中移除该订阅；
  5. 启动时立即执行一次，之后按 `check-interval` 周期执行。
- 规则文本无有效条目时不加载（避免空订阅覆盖已有数据）。

## 6. 配置契约

- `config.yml`：`server.http`(9898)、`server.address`、`server.token`、
  `database.type`(sqlite)、`persist.ban-logs-keep-days`(180)、`persist.banlist`(true) 等。
- `profile.yml`：`check-interval`(5000)、`ban-duration`(1209600000=14 天)、
  `ignore-peers-from-addresses`、`module.*`（各模块 enabled/ban-duration/规则集）。
  本阶段把 profile 段并入 `config.yml` 的 `profile:` 下（单文件部署），字段名与语义不变。
- **[GOLDEN] 模块启用语义**（`AbstractFeatureModule.shouldModuleEnabled`）：
  `module.<name>` 配置节缺失，或节内没有 `enabled: true` → **该模块被禁用**。
- **[GOLDEN] `ban-duration` 语义**：模块级 `ban-duration` 缺省 / 为 0 → 回退到全局 `ban-duration`。
- **[GOLDEN] 模块注册顺序**（`PeerBanHelper.registerModules`）：
  `ip-address-blocker → peer-id-blacklist → client-name-blacklist → progress-cheat-blocker`，
  该顺序是「等级与时长并列」时的最终裁决依据。
- 出厂 `default-config.yml` 必须与上游 `profile.yml` 等价（模块全开、规则集逐字一致、
  参数一致），由 L2 黄金测试锁定。
- Rust 版读取同样的 YAML 字段；缺失键使用与上游相同的默认值。

## 6.1 文案国际化（i18n）**[GOLDEN]**

对齐上游 `TranslationComponent` + `TextManager.tl` + `MsgUtil.fillArgs`：

- 可翻译文本 = `key` + 位置参数；参数本身也可以是可翻译文本（递归渲染）。
- 文案表**逐字复用**上游 `src/main/resources/lang/*`：`en_us` / `zh_cn` / `zh_tw` +
  `messages_fallback`（内嵌进二进制），并支持 `data/lang/<locale>/messages.yml` 运行时覆盖。
- 渲染流程：
  1. locale 归一化（小写、`-` → `_`）；
  2. 查表回退链 `locale → en_us → messages_fallback`；查不到则**用 key 本身**；
  3. 把模板中的 `{}` 按位置替换（参数不足保留 `{}`，多余参数忽略）。
- 抽查使用的百分比格式为 `DecimalFormat("0.00%")`（×100、两位小数、附 `%`）。
- `CheckResult.rule_key` / `reason_key` 保存上游 `Lang` 键与参数（如
  `PCB_RULE_REACHED_MAX_DIFFERENCE`、`MODULE_PCB_PEER_BAN_INCORRECT_PROGRESS`），
  落库/日志按 `language.locale`（默认 `zh_cn`）渲染后写入 `rule` / `reason`。
- 上游已知怪癖需保留：`PeerIdBlacklist` 的 reason 复用 `MODULE_CNB_MATCH_CLIENT_NAME` 文案键。

## 7. Web API 契约（本阶段实现的子集）

- 鉴权：除 OOBE/健康检查与 blocklist 端点外，请求需带 Token（`Authorization: Bearer` 或查询参数/头）。
- **封禁列表端点（匿名，对齐上游 `Role.ANYONE`）**：供下载器拉取，内容取内存封禁表并
  **始终按支持范围封禁**做 `remapBanListAddress`（IPv6 `/52` 默认开启）：
  - `GET /blocklist/p2p-plain-format`：每行 `<32位随机十六进制>:<start>-<end>`；
    列表为空且 `User-Agent` 以 `Transmission` 开头时返回
    `TransmissionWorkaround:127.127.127.127-127.127.127.127`（否则 Transmission 会拒绝空列表）；
  - `GET /blocklist/ip`：每行一个 CIDR；
  - `GET /blocklist/dat-emule`：每行 `<start> - <end> , 000 , <cidr>`。
  - 主机地址（无前缀）按 `/32`、`/128` 处理（对齐 `toPrefixBlock()`）。
- `GET /api/ban/list`：当前封禁列表。
- `GET /api/ban/logs`：封禁历史（分页）。
- `GET /api/metrics/general`：概要统计（下载器数、torrent 数、peer 数、封禁数）。
- `GET /api/downloaders`：下载器列表与状态。
- 静态资源：`/` 托管 WebUI（`data/static`，缺失时返回占位页）。
- 统一响应体 `{success, message, data}`。

## 8. 持久化契约

SQLite（默认 `data/persist/peerbanhelper.db`），核心表：

- `ban_logs`：id、downloader_id、torrent_hash/name、ip、port、peer_id、client_name、module、
  reason、ban_duration、created_at（按 `ban-logs-keep-days` 清理）。
- `banned_ips`：ip、首次/最近封禁时间、命中模块、累计次数、`ban_until`（解封时间）。
  进程启动时把**未到期**的条目载入内存封禁表；解封时同步删除该行。
  （上游同时持久化 `banForDisconnect` 等元数据，Rust 阶段暂只保留恢复所需的字段。）
- `progress_peer_history`：downloader、torrent、ip、uploaded 快照、最大进度、时间（供虚假进度模块）。

## 9. 已知差距（阶段性，按对“忠实”影响排序）

以下差距已确认，按影响从高到低排列，作为后续迭代的输入：

> 上游全部规则模块（含默认启用与默认关闭）均已实现并接入流水线：
> `ip-address-blocker`（含 GeoIP 四维度）/ `peer-id-blacklist` / `client-name-blacklist` /
> `expression-engine` / `progress-cheat-blocker` / `multi-dialing-blocker` / `auto-range-ban` /
> `btn` / `ip-address-blocker-rules` / `anti-vampire` / `ptr-blacklist` /
> `idle-connection-dos-protection`。
> 所有「需外部资源」的模块**均已忠实移植**，并在资源缺失时按上游语义降级为空操作（不封禁、不报错）。

> **已关闭**：~~监控数据未持久化~~ —— `active-monitoring` / `peer-analyse-service.*` 的落点已从内存实现
> 换成 `pbh-db::DbMonitorSink`（与其余持久化共用同一个 `Database`），表结构逐条对齐上游 SQLite 迁移脚本：
> `alert`（上游为单数表名）/ `traffic_journal_v3` / `peer_connection_metrics(_track)` / `peer_records` /
> `tracked_swarm` / `torrents`；`peer_records.peer_geoip` 由 sink 内查 IP 库填充。
> Web 侧已暴露 `/api/modules/swarm-tracking`（裸 `{"trackedSwarmSize": N}`）、
> `/api/modules/swarm-tracking/details`（`page`/`pageSize` + `orderBy`，`{page,size,total,results}`）、
> `/api/alerts`（未读告警，按请求 locale 渲染），以及告警读写端点
> （`PATCH /api/alert/{id}/dismiss` / `POST /api/alert/dismissAll` / `DELETE /api/alert/{id}`）
> 与阈值告警的 `push:` 渠道推送。
> （这四类模块上游即非 `RuleFeatureModule`，`check` 恒 `pass()`，**不影响任何封禁决策**。）

1. **BTN 传输层已移植并接线**：`pbh-core::btn_transport` 提供配置端点握手、协议版本校验、
   `X-BTN-ContentVersion` + 本地缓存（落 `meta` 表）、PoW captcha 与到期调度，并在 `pbh/src/main.rs`
   接线（默认禁用 ⇒ 零网络）。abilities（`submit_*` / `heartbeat` / `ip_query` / `reconfigure`）
   全部构造并调度；上报数据源 `pbh-db::DbBtnSubmitSource`（`history` / `tracked_swarm` / `peer_records`），
   `GET /api/peer/{ip}/btnQuery` 已接 `BtnNetwork::query_ip`。BTN 脚本规则在
   `btn.allow-script-execute: true` 时以 rhai 执行（上游默认 `false`）。
   未注入规则时恒 `pass()`（未配置 BTN 服务端的部署与上游行为一致）。
2. **GeoIP 数据库自动更新已移植**：`pbh-core::geoip_update` 三镜像轮换下载 + XZ 解压 +
   45 天 mtime 间隔 + 校验后原子替换，在 `GeoIpDb::load` 之前接线（对齐上游「先 updateMMDB 再 loadMMDB」）；
   `auto-update: false`（默认）⇒ 严格 no-op。数据库缺失/损坏或 `pbh.forceDisableIPDB` 时
   四个维度全部不命中（对齐上游无库行为）。
3. **AutoSTUN 次要能力已移植**：UDP NAT 类型探测（对齐 cdnbye/上游 `StunManager`，仅 WebUI/遥测展示）
   与 TCP 转发器 + 端口保活 + 友好回环映射绑定均已实现；`enabled: false`（默认）⇒ 严格 no-op。
   上传限速已落地：`Downloader` trait 暴露 `getSpeedLimiter` / `setSpeedLimiter`，
   `active-monitoring` 的滑动窗口限速在 `enabled: true` 时真正下发（默认关闭，默认配置下无差异）。
4. **`expression-engine` 语法翻译表已补全**：rhai 引擎与返回值语义已对齐
   `ScriptEngineManager.handleResult`；AviatorScript → rhai 的 API 映射、字段表、语法对照与迁移示例
   见 `docs/expression-engine-migration.md`。

> **已实现、且默认配置下不改变封禁语义**：
> - `expression-engine`：默认启用、默认空脚本目录 ⇒ 无封禁。
> - `ptr-blacklist` / `idle-connection-dos-protection`：默认关闭（对齐上游 `profile.yml`），显式启用即生效。
> - `auto-stun`：默认 `enabled: false` ⇒ 严格直通，不发起任何网络请求。
> - 下载器适配器：qBittorrent / Transmission / Deluge / BiglyBT / BitComet / Aria2Next。
> - 告警推送 9 渠道：未配置渠道时为无操作；上游只在「bypass 地址 + 疑似 NAT 配置错误」处告警，
>   并非每次封禁都推送。
> - **Web API 按请求 locale 渲染 + `rule`/`reason` 结构化存储**：`ban_logs` 接受 `?locale=`，
>   用落库的 `TranslationComponent`（JSON）重新本地化；无 key 时回退到服务端渲染文案。
>   `BanLog` 新增 `rule_key` / `reason_key` 列（含增量迁移），与上游“按请求 locale 本地化”一致。

## 10. 不在本阶段范围（明确边界）

桌面 GUI、插件系统（WASM/wasmtime）、WebSocket 实时推送、多数据库（MySQL/PostgreSQL）、
与 Java 版并行灰度（L5 双跑 diff）。
这些在 PLAN.md 后续阶段定义，不得在未定义契约前提前改变本阶段行为。
