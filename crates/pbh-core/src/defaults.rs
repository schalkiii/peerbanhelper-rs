//! 上游 `profile.yml` / `config.yml` 的内置默认值，逐字对齐 v9.5.1。

/// peer-id-blacklist 默认规则（profile.yml `banned-peer-id`，JSON 文本原样保留，顺序不可调整：
/// `matchRule` 上报的是**最后一条**命中 TRUE 的规则）。
pub const DEFAULT_BANNED_PEER_ID: &[&str] = &[
    r#"{"method":"STARTS_WITH","content":"-hp"}"#,
    r#"{"method":"STARTS_WITH","content":"-xm"}"#,
    r#"{"method":"STARTS_WITH","content":"-dt"}"#,
    r#"{"method":"CONTAINS","content":"-rn0.0.0"}"#,
    r#"{"method":"STARTS_WITH","content":"-sd"}"#,
    r#"{"method":"STARTS_WITH","content":"-xf"}"#,
    r#"{"method":"STARTS_WITH","content":"-qd"}"#,
    r#"{"method":"STARTS_WITH","content":"-bn"}"#,
    r#"{"method":"STARTS_WITH","content":"-dl"}"#,
    r#"{"method":"STARTS_WITH","content":"-ts"}"#,
    r#"{"method":"STARTS_WITH","content":"-fg"}"#,
    r#"{"method":"STARTS_WITH","content":"-tt"}"#,
    r#"{"method":"STARTS_WITH","content":"-nx"}"#,
    r#"{"method":"CONTAINS","content":"cacao"}"#,
    r#"{"method":"EQUALS","content":"Unknown"}"#,
    r#"{"method":"EQUALS","content":"未知"}"#,
];

/// client-name-blacklist 默认规则（profile.yml `banned-client-name`，顺序不可调整）。
///
/// 倒数第 3 条的 content 是上游 profile.yml 第 92 行的真实字节 `\xde\xad__`
/// （即 U+07AD 后跟两个下划线，注释里写的 `0xde-0xad-0xbe-0xef` 是误导）。
pub const DEFAULT_BANNED_CLIENT_NAME: &[&str] = &[
    r#"{"method":"STARTS_WITH","content":"hp/torrent"}"#,
    r#"{"method":"STARTS_WITH","content":"hp "}"#,
    r#"{"method":"STARTS_WITH","content":"dt/torrent"}"#,
    r#"{"method":"STARTS_WITH","content":"dt "}"#,
    r#"{"method":"STARTS_WITH","content":"xm/torrent"}"#,
    r#"{"method":"STARTS_WITH","content":"xm "}"#,
    r#"{"method":"STARTS_WITH","content":"taipei-torrent"}"#,
    r#"{"method":"CONTAINS","content":"rain 0.0.0"}"#,
    r#"{"method":"CONTAINS","content":"gopeed dev"}"#,
    r#"{"method":"STARTS_WITH","content":"xfplay"}"#,
    r#"{"method":"CONTAINS","content":"StellarPlayer"}"#,
    r#"{"method":"CONTAINS","content":"SP "}"#,
    r#"{"method":"CONTAINS","content":"flashget"}"#,
    r#"{"method":"CONTAINS","content":"tudou"}"#,
    r#"{"method":"CONTAINS","content":"torrentstorm"}"#,
    r#"{"method":"CONTAINS","content":"qqdownload"}"#,
    r#"{"method":"STARTS_WITH","content":"qbittorrent/3.3.15"}"#,
    r#"{"method":"STARTS_WITH","content":"github.com/thank423/trafficconsume"}"#,
    "{\"method\":\"STARTS_WITH\",\"content\":\"\u{07AD}__\"}",
    r#"{"method":"STARTS_WITH","content":"Unknown ["}"#,
    r#"{"method":"STARTS_WITH","content":"Gopeed bt-exp"}"#,
];

/// 跳过检查的地址（bypass），profile.yml `ignore-peers-from-addresses`
pub const DEFAULT_IGNORE_ADDRESSES: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "fc00::/7",
    "100.64.0.0/10",
    "169.254.0.0/16",
    "127.0.0.0/8",
    "fe80::/10",
];

/// 全局默认封禁时长：14 天（ms）
pub const DEFAULT_BAN_DURATION_MS: i64 = 1_209_600_000;
/// PeerId/ClientName 黑名单默认封禁时长：3 天（ms）
pub const BLACKLIST_BAN_DURATION_MS: i64 = 259_200_000;
/// PCB 默认封禁时长：30 天（ms）
pub const PCB_BAN_DURATION_MS: i64 = 2_592_000_000;
/// ban wave 间隔：5 秒（ms）
pub const DEFAULT_CHECK_INTERVAL_MS: u64 = 5_000;

/// ProgressCheatBlocker 默认参数
pub mod pcb {
    pub const MINIMUM_SIZE: i64 = 50_000_000;
    pub const MAXIMUM_DIFFERENCE: f64 = 0.10;
    pub const REWIND_MAXIMUM_DIFFERENCE: f64 = 0.07;
    pub const BLOCK_EXCESSIVE_CLIENTS: bool = true;
    pub const EXCESSIVE_THRESHOLD: f64 = 1.5;
    pub const IPV4_PREFIX_LENGTH: u8 = 32;
    pub const IPV6_PREFIX_LENGTH: u8 = 56;
    /// 是否把 PCB 状态落库（上游 `enable-persist: true`）
    pub const ENABLE_PERSIST: bool = true;
    pub const PERSIST_DURATION_MS: i64 = 1_209_600_000;
    pub const MAX_WAIT_DURATION_MS: i64 = 30_000;
    /// 快速 PCB 测试：上游 profile.yml 默认启用（0.1 = 10%），断开时长 15000ms
    pub const FAST_PCB_TEST_PERCENTAGE: f64 = 0.1;
    pub const FAST_PCB_TEST_BLOCK_DURATION_MS: i64 = 15_000;
}

/// qBittorrent 适配器默认
pub mod qb {
    pub const MAX_CONCURRENT_SLOTS: usize = 128;
    pub const PAGE_SIZE: u32 = 100;
    pub const CONNECT_TIMEOUT_SECS: u64 = 10;
    pub const READ_TIMEOUT_SECS: u64 = 30;
    pub const MIN_SUPPORTED_VERSION: &str = "4.5.0";
}
