# 测试覆盖矩阵与用例清单

> 生成于 2026-09-25（commit ca3ef8a 之后）。统计口径：`cargo test --workspace -- --list`，
> 共 **23 个测试目标、587 个用例**。运行：`cargo test --workspace`；
> mock 对跑：`pwsh -File java_dualrun.ps1 -Fixture <场景>.json -Tag <tag>` 后接
> `rust_dualrun.ps1 -Fixture <场景>.json -Tag <tag>`（需先构建 `target\debug\{pbh,mockqb}.exe`）。

## 1. 分层模型

| 层级 | 位置 | 说明 |
|---|---|---|
| L1 单元 | 各 crate `#[cfg(test)]` | 纯函数/结构级，全确定性 |
| L2 模块黄金 | `crates/pbh-golden/tests/l2_*.rs` | 单模块判定对齐上游语义（含多波状态机） |
| L2 解析黄金 | `crates/pbh-core/tests/*_golden.rs` | 词法/格式解析对齐上游行为 |
| L3 下载器集成 | `crates/pbh-golden/tests/l3_*.rs` | HTTP mock 下验证登录/peers 解析/封禁下发载荷 |
| L4 端到端 | `crates/pbh-golden/tests/l4_end_to_end.rs` | 完整流水线（mock 下载器 → 判定 → 封禁） |
| L5 mock 对跑 | 仓库根 `java_dualrun.ps1` + `rust_dualrun.ps1` | Java/Rust 连同一 mockqb，封禁集合逐字 diff |

## 2. 模块 × 覆盖矩阵

图例：✅ 充分（≥6 用例且关键分支覆盖）｜🟡 有覆盖但有已知缺口｜❌ 对跑未覆盖

| 模块 | L1 单元 | L2 黄金 | L3/L4 | L5 对跑 | 小计 |
|---|---|---|---|---|---|
| btn（传输层/ability/上报）| ✅ 56 | — | — | ❌ | 61 |
| monitor（监控模块宿主）| ✅ 45 | — | — | n/a¹ | 45 |
| downloader（qb/tr/aria2/biglybt/bitcomet/deluge）| ✅ 91+ | — | ✅ 20 | ✅ sample | 118 |
| geoip（IPDB/GeoCN/更新器）| ✅ 37 | — | — | n/a² | 37 |
| remap / 地址翻译 | ✅ 24+ | ✅ 8 | — | ✅ matrix | 36 |
| core 其它（iputil/pipeline/i18n/banlist/auto_stun…）| ✅ 105+ | ✅ 13+ | — | — | ~125 |
| db（schema/history/torrents/统计）| ✅ 26 | — | — | ✅ compare_dualrun | 31 |
| config（AppConfig/上游布局）| ✅ 15 | ✅ 1 | ✅ 2 | — | 18 |
| string_blacklist（client/peer-id）| ✅ 3 | ✅ 12 | — | ✅ matrix | 15 |
| web（API/鉴权/任务）| ✅ 14 | — | — | n/a | 14 |
| progress_cheat（PCB）| ✅ 1 | ✅ 11 | — | ✅ rewind/desync/excessive | 12 |
| expression_engine | ✅ 11 | ✅ 6 | — | n/a³ | 17 |
| ip_blacklist | ✅ 9 | ✅ 1 | — | ✅ matrix | 10 |
| ptr_blacklist | ✅ 9 | ✅ 1 | — | ❌ | 10 |
| mockqb（波次轮转）| ✅ 3 | — | — | 载体 | 3 |
| downloader-tr av 脚本 | — | ✅ 10 | — | — | 10 |
| ip_rule_list | ✅ 内嵌 | ✅ 22 | — | ✅ sample（all-in-one）| 22+ |
| multi_dialing | ✅ 1 | ✅ 5 | — | ✅ sample/replay | 6 |
| idle_protection | — | ✅ 6 | — | ❌ 待补 | 6 |
| banlist | ✅ 4 | ✅ 1 | — | 间接 | 5 |
| pipeline（顺序/聚合）| ✅ 2 | ✅ 3 | — | 间接 | 5 |
| anti_vampire | — | ✅ 2（分支全覆盖）| — | ✅ vampire（新增）| 2+ |
| auto_range_ban | — | ✅ 3 | — | ✅ arb（新增）| 3 |

¹ 监控模块不参与 peer 判定，对跑无意义。² GeoIP 由城市/ASN 场景间接覆盖（matrix 城市 IP）。
³ 表达式脚本无上游 Java 对应启用场景，对跑默认关闭。

## 3. mock 对跑用例清单（`crates/pbh-mockqb/fixtures/`）

| fixture | 场景 | 覆盖断言 | 状态 |
|---|---|---|---|
| `sample.json` | 多拨（同 /24 三 IP）+ PeerId `-hp` 前缀 + 正常对照 | 两侧封禁集合与理由一致 | ✅ 一致 |
| `pcb_rewind.json` | PCB 进度回退跨波（0.90 → 0.45，rewindProgress）| 同波封禁、同模块、同格式 | ✅ 一致 |
| `pcb_desync.json` | PCB deSync 开窗状态机（差值 20% → 30s 窗口 → 封禁）| 窗口过期后同波封禁 | ✅ 一致 |
| `pcb_excessive.json` | PCB 过量下载（200% / 90% vs 150% 阈值）| .91 两侧同封 | 🟡 见 pending-1 |
| `modules_matrix.json` | 10 peer 矩阵：CIDR/单 IP/端口/城市×2/REGEX/PeerId/握手豁免/连锁/双对照 | 12/12 全「共有」 | ✅ 一致 |
| `anti_vampire.json`（新增）| 迅雷 preset 全分支：非 0019 封 / 0019+做种封 / 0019 下载中放行 / 对照 | 集合一致 | ✅ 一致 |
| `auto_range_ban.json`（新增）| wave0 单 IP 封禁 → wave1 同 /30 连锁 | 4/4 全「共有」 | ✅ 一致 |
| `replay_real.json`（生成器 `gen_replay_fixture.py`，产物不入库）| Java history 真实封禁 peer 重放 | 99.3%（954/961）一致 | ✅ 见既有归因 |

规则注入：`inject_test_profile.py` 向两侧配置写入同一测试规则集
（CIDR `203.0.114.0/24`、单 IP `198.51.100.200`、端口 `39999`、城市 `浙江省 温州市`、
client-name REGEX `^EvilClient.*`），幂等、缩进跟随序列既有项。

## 4. 长跑对账记录与已知缺口

### 4.1 长跑对账（36h 窗口，2026-09-24 12:45 → 09-26 00:47 UTC，wave#1087）

Rust 16 条封禁 vs Java 差异 11+9+1（端口粒度）/ 9+7+1（`--ip-only`）。**全部归因为
观测干扰，零判定逻辑缺陷**：

| 归因 | 条数 | 说明 |
|---|---|---|
| 级联前置缺失 | ~4 | Java 的 `IPv6/47` 连锁 / AutoRangeBan 依赖其先封的种子 IP，Rust 无该种子则不连锁（种子本身是相位差漏封） |
| 订阅/拒列表版本相位差 | ~2 | BTN denylist、all-in-one 订阅动态更新，两侧快照时点不同 |
| PCB rewind 观测相位差 | ~2 | 进度快照时点不同（Java 23:12 判 rewind 时 Rust 23:11/14:51 的快照无倒退） |
| 纯相位差（互有先封）| ~2 | 双向对称（Java 独有 7 IP / Rust 独有 3 IP，互有先到先封），非单侧缺陷 |
| 级联路径差异 | 1 | `2409:8a3c:...:d44`：Java=AutoRangeBan / Rust=MultiDialingBlocker（同段防护，触发路径不同，语义等价） |

对账工具已加 `--ip-only` 模式（IP 粒度消除端口伪差异；长跑中同一 IP 两侧常封到
不同端口的连接）。

**RangeKey 修复后续验证（2026-10-06）**：新 epoch（10-05 16:21 UTC，含两项修复的
release）窗口 11.3h 对账——**module-mismatch 为 0**（修复前 26 条全部消失，归因标签
一致）；Rust 20 条独有均为修复前旧二进制的 PCB excessive 小时级循环（最后一条
10-06 02:05，早于修复部署 03:12）；**修复部署后（03:12+）history 零新增**，
循环封禁消失确认。

### 4.1.1 长跑对账（10.9 天窗口，→ 2026-10-05 10:39 UTC，wave#7853）

窗口内封禁行 Java 196 / Rust 178；IP 去重 Java 177 / Rust 95 / 交集 79（Jaccard 40.9%）。
**差异归因**：

- **🆕 pending-5（已定位并修复，2026-10-05）**：Rust 独有 **46 行 PCB excessive**（Java 对同 IP
  零封禁，且 Java 观测 `108.163.157.206` uploaded_max=7.03G 仍不封——其 torrent size
  较大，绝对值未达 1.5× 阈值）。Rust 的 `computed_uploaded` 含 `tracking_uploaded_increase_total`
  累计，在**长周期 + peer 断连重连回绕**（`uploaded < last` → 全量重计）与
  **IPv6 同段共享 range 实体**（同段其它 IP 的增量也计入 max）下**虚高**，导致
  excessive 误封。上游公式相同（`ProgressCheatBlocker.java:236`），但 Java 对同 IP
  同输入不封——**需对照两侧 tracking 序列与 torrent 维度**定位偏差点。
  修复（两处，2026-10-06）：① on_unban 对齐上游——保留内存判定基线（last_report_uploaded），仅由 wave 层删 DB 行；② **RangeKey 加 ip 分量**（对齐上游缓存键 ProgressCheatBlocker.java:220 含 ip/port）——range 实体为「每 IP 独立副本」而非同段共享，否则多拨段下 computed_uploaded 被同段其它 IP 的增量推高（此为 46 行误封的主因）；持久化加载跳过 range 行（无 ip 列无法还原副本键，重启后 range 从零重计，对齐上游 cache TTL 行为）。新增单元回归 pcb_fast_test_then_excessive_does_not_double_count_constant_uploaded。修复后 pcb_excessive 对跑复验：.92 不再误封（仅 fast test 全量重放，与 Java 一致），.91（真 excessive 2G）仍正常封禁。
- **module-mismatch 26 条**：全部集中在 `240e:f7:c000:311::/56` 多拨重灾段，
  **两侧都封了这些 IP**（IP 级一致），仅归因模块不同（Java=AutoRangeBan 为主、
  Rust=MultiDialingBlocker 为主，双向少数）——多拨与连锁在同段交替先触发的
  时序差异，IP 级行为一致，非缺陷。
- 其余为既有归因模式（级联前置缺失/订阅版本相位差/rewind 观测相位差/纯相位差）。

**稳定性结论更新**：**10.9 天连续运行**（wave#7853）无冻结、无重启、双 200，
数月级稳定性风险大幅收敛；内存/句柄无泄漏迹象（采样持续记录 RSS）。

**冻结静态审计（2026-10-06，排除清单）**：
- HTTP 超时齐备：下载器 `connect_timeout + timeout`（http.rs:118-119）、
  BTN `callTimeout 60s`（btn_transport.rs:282）——排除「无超时 HTTP 永久阻塞」
- 清理分支锁序干净：pcb 清理（main.rs:654-667）`store.lock` 与 `db.cleanup_pcb`
  不嵌套；无 conn → store 反向嵌套
- record_bans 锁序干净：`insert_history`（conn）与 `ban_list().lock` 不嵌套
- PCB store 仅在 progress_cheat.rs 内部使用，不与 conn 交叉
→ 静态审计未发现死锁环；根因定位需 dump 线程栈分析（pending-6，
  `pbh-frozen-1006-001212.dmp`）。另疑点：冻结时点 16:11 与 8h 周期的
  PCB 清理调度启动时刻吻合（16:21 前的周期为 08:21/16:21/00:21），
  但清理分支锁序干净，更可能是同时刻的其它定时任务交错，待 dump 确认。

**pending-6 dump 分析结论（2026-10-07）**：两个冻结样本（1006 长跑、1007 生产）均无法
符号化——release profile `strip = true` 不生成 PDB；带 debuginfo 重编译会使代码布局
漂移（.text 大小差 54KB，debuginfo 影响优化决策），旧 dump 偏移不适用。dump 只能
确认「主线程停在内核等待、worker 线程全部 idle park、web 线程存活」的结构。

**生产部署后的「冻结循环」重新定性（2026-10-07，重要更正）**：GUI 部署当日出现的
「pbh 每 ~100s 冻结一次」**不是 pbh 缺陷**，而是**外部看门狗误杀**——初版看门狗按
「CPU 增量 < 0.5s/30s」判定 wave 冻结，但生产 config 的 wave 间隔是 **120s**（长跑
实例为 5s），main 线程在 wave 间空闲 ~107s，30s 窗口内 CPU 增量恒低于阈值，健康
进程被周期性误杀（GUI 自动重启掩盖了进程更替）。看门狗已改为 **wave 完成心跳**
（读 pbh-gui.log 的「主循环返回」打点，阈值 300s），并要求 wave 间隔必须与部署
config 一致地配置阈值。修正后 pbh 的 wave/flush/PCB 清理/BTN 全链路日志完整正常，
无真冻结复现。

**给 wave 主循环加全链路 phase 打点（2026-10-07，诊断基建）**：`wave#N 开始/主循环返回`、
run_once 的 5 个阶段（解封/判定/落库/下发/PCB 落库）、run_downloader 的 login/fetch_torrents/
并发拉取、PCB 8h 清理前后——均为 DEBUG 级（默认 `pbh=debug` 已可见）。下次出现真冻结，
日志最后一条即精确卡点。GUI 侧配套修复：子进程 stdout/stderr 重定向到 `data/pbh-gui.log`
（此前 GUI 无控制台导致日志全丢）、监督线程把子进程退出码写入日志（区分正常退出 /
access violation / 栈溢出）。

**pending-6 现状**：36h 长跑冻结发生在 a384daa 修复之前；修复后长跑 13.6h+（wave#409）
与生产部署均无真冻结复现，**大概率已随 a384daa 解决**。两个 dump 因符号缺失无法回溯
确认；保留 phase 打点作为后续冻结的定位手段，pending-6 降级为「观察项」。

### 4.2 外部仓库 workflow 处置（2026-10-05）

`schalkiii/PeerBanHelper`（Java 版 fork）的「Update IPDB on COS」定时任务连续失败：
① `ljxi/GeoCN` 上游取消 `Latest` release tag → GeoCN.mmdb 下载 404（已修复：
改用 `releases/latest/download/` 动态路径，commit e32e080）；② fork 不继承上游
Actions Secrets，COS 上传缺 `TENCENT_CLOUD_COS_PBH_STATIC_*` 密钥必失败（该任务
为上游维护者更新自家镜像用，fork 无需）→ 已禁用该 workflow（`gh workflow disable`，
保留文件可随时重新启用）。**本项目 Rust 的 GeoIP 更新走官方
`PBH-BTN/GeoLite.mmdb` + 上游维护者镜像，不受影响。**

### 4.3 已知缺口与 pending

| 编号 | 事项 | 状态 |
|---|---|---|
| ~~pending-1~~ | ~~PCB 过量下载累计在 `BanForDisconnect` 后翻倍~~ **已解除（2026-09-25）**：
   补充取证证实 **Java 同样封禁 `.92`**（经全量重放下发，`BanForDisconnect` 静默不打
   封禁日志导致此前误判"Java 不封"）。1.8G = fastPcbTest 断开重连后重计的
   **共同上游语义**，两侧封禁集合逐字一致 | ✅ 已验证一致 |
| ~~pending-2~~ | ~~`idle_protection` 对跑~~ **已补齐并一致（2026-09-25）**：
   `idle_protection.json`（做种 torrent + 恒 0 速度 + 恒 progress，加速参数
   speed 1e9 / max-idle 3s 注入），两侧同波命中 `idleTimeout`，封禁集合一致。
   教训：fixture 需遵守模块前置语义——progress 非 0 会在
   `reset-on-status-change` 量纲（×100）下每波重置计时 | ✅ 一致 |
| ~~pending-3~~ | ~~Java OOBE 覆盖注入的 profile~~ **已根治（2026-09-25）**：
   双修复——① OOBE body 缺 `basicAuth` 对象导致
   `QBittorrentConfigImpl.saveToYaml` NPE、下载器无法持久化（补空对象）；
   ② 改为二段启动（OOBE 完成后停 Java → 重新注入 → 二段启动跑波，
   顺序消除 OOBE 的 profile 覆盖）| ✅ 已根治 |
| pending-4 | `ptr_blacklist` 无对跑用例（需 PTR 服务器 mock，成本较高、单元已覆盖解析与缓存）| 🟡 待补 |

## 5. 生产部署替代 Java 版评估（2026-09-25）

**结论：核心封禁判定链路已具备生产替代条件，建议以「灰度并行」方式迁移。**（2026-09-25 更新：mock 对跑 7 场景全部一致，pending-1/2/3 已收敛，仅剩 pending-4）

| 能力面 | 状态 | 说明 |
|---|---|---|
| 规则模块（13 个）| ✅ | 全部移植；判定保真经 mock 对跑（6 场景）+ 真实数据重放（99.3%）双验证 |
| 下载器（6 种）| ✅ | qBittorrent/Transmission/aria2/BiglyBT/BitComet/Deluge，L3 载荷级测试 |
| 数据库 | ✅ | 与上游同 schema，可直接共用 Java 的 data 目录（含 history/PCB 状态） |
| 配置 | ✅ | 兼容上游 `config/config.yml + profile.yml` 双文件布局与单文件布局 |
| GeoIP | ✅ | 三镜像自动更新 + GeoCN 区划内嵌（城市规则已对齐） |
| 推送（6 渠道）| ✅ | SMTP/Gotify/Ntfy/PushPlus/PushDeer/Bark |
| BTN | ✅ | 传输层/ability/上报 61 用例；长跑实机启用中 |
| WebUI | ✅/🟡 | 后端 API + 静态托管已就绪；**前端需复制上游 `webui/dist` 到 `data/static`**（部署步骤，非代码缺口） |
| 迁移方式 | ✅ | 复制 Java `data` 目录 → 替换进程；配置/DB/GeoIP 库原样可用 |
| 稳定性 | 🟡 | 实机长跑 10.9 天中发生两次冻结（8.5h 已修；36h 新发：整进程 CPU 归零但 web 线程存活，minidump 已取证 	arget/live/pbh-frozen-1006-001212.dmp）；冻结根因分析为生产替代前置项，已用含修复的 release 重启 |
| 自更新 | ❌ | PBH 自身更新器未移植（Java 有），需手动替换二进制 |
| OOBE 向导 | 🟡 | Rust 无向导页（首启生成 token），迁移场景不受影响 |

**建议迁移步骤**：① 停 Java → ② 复制 `data` 目录 → ③ 放置 `webui/dist` 到
`data/static` → ④ 启动 Rust（`pbh.exe --data <dir>`）→ ⑤ 与 Java 并行期用
`--dry-run` 或备份实例观察一周 → ⑥ 切换。

## 5. 已固化的行为语义（对跑沉淀，两侧一致属上游行为）

1. **握手豁免**：up/dl 全 0 = 握手中，`IpBlacklist`/`PCB`/`MultiDialing`/`IPBlackRuleList`
   等直接跳过；**`PeerIdBlacklist` 例外**（握手**且** peerId 为空才豁免）；
   **`AntiVampire` 无豁免**（全 0 速度照样判定）。
2. **REGEX 整段 matches() 语义**：规则需写 `^EvilClient.*` 而非 `^EvilClient`。
3. **多拨收敛路径**：上游串行语义为「同波只封超过容忍值的那个，其余下一周期补封」；
   Java 的 ForkJoinPool 并发竞态可能同波全封（非确定），最终集合收敛一致。
4. **封禁列表字符串化**：全量下发对每个 IPv4 附带 hex 压缩 mapped IPv6 变体
   （`toCompressedString()`，见 `remap.rs::to_compressed_string`）；
   增量下发用下载器原始 `ip:port`。
