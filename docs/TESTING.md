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

## 4. 已知缺口与 pending

| 编号 | 事项 | 状态 |
|---|---|---|
| pending-1 | **PCB 过量下载累计在 `BAN_FOR_DISCONNECT` 后翻倍**：`pcb_excessive` 场景中
   `.92`（uploaded 恒 0.9G）wave1 命中 fastPcbTest（BanForDisconnect，不下发），
   wave2 起 Rust 以累计 1.8G 判 excessive 封禁并下发，Java 保留基线（0.9G）不封。
   疑点在 wave 层对 `BanForDisconnect` 的处理链（`on_unban` 删实体重建后
   `last_report_uploaded` 归零 → 恒定 uploaded 被重复累计）。真实流量（uploaded 单调递增）
   下的严重度待评估 | 待查（`crates/pbh/src/wave.rs` 断连处理链）|
| pending-2 | `idle_protection` 无对跑用例（需跨波 idle 计时 fixture，输入含 flags/速度演化）| 待补 |
| pending-3 | Java dualrun 的 OOBE 时序竞态：OOBE 正常完成会以内存默认覆盖已注入的
   profile.yml（NPE 轮反而保留）；已加「等待生成 + 失败即中止」，注入移到 OOBE 后
   的方案待做 | 待补 |
| pending-4 | `ptr_blacklist` 无对跑用例（需 PTR 服务器 mock，成本较高、单元已覆盖解析与缓存）| 待补 |

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
