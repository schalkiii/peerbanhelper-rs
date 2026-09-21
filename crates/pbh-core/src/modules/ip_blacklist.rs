//! IP/端口黑名单模块（`ip-address-blocker`）。
//!
//! 判定顺序对齐上游 `IPBlackList#shouldBanPeer`：
//! 握手放行 → 端口 → IP/CIDR → GeoIP（ASN → 地区 → 网络类型 → 城市）→ pass。
//! GeoIP 数据由 [`GeoIpProvider`]（≈ 上游 `IPDBManager`）提供；数据库不可用时四个维度全部不命中。

use crate::geoip::{GeoIpConfig, GeoIpProvider};
use crate::i18n::TranslationComponent;
use crate::iputil::{self, IpSet};
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct IpBlacklist {
    pub ips: IpSet,
    pub ports: HashSet<u16>,
    /// 0 = 使用全局封禁时长
    pub ban_duration_ms: i64,
    /// GeoIP 四维度配置（对齐 `IPBlackList` 的 `asns` / `regions` / `cities` / `networkType`）
    pub geo: GeoIpConfig,
    /// GeoIP 数据源（对齐 `@Autowired IPDBManager`）。
    /// `None` ⇒ 数据库不可用，四个 GeoIP 维度恒不命中（与上游无 GeoIP 库时一致）。
    pub geo_provider: Option<Arc<dyn GeoIpProvider>>,
}

impl IpBlacklist {
    pub fn new(ips: &[String], ports: &[u16], ban_duration_ms: i64) -> Self {
        Self::with_geo(ips, ports, ban_duration_ms, GeoIpConfig::default())
    }

    /// 带 GeoIP 维度配置构造（数据源仍需 [`IpBlacklist::with_provider`] 注入）。
    pub fn with_geo(ips: &[String], ports: &[u16], ban_duration_ms: i64, geo: GeoIpConfig) -> Self {
        Self {
            ips: IpSet::from_cidrs(ips.iter().map(|s| s.as_str())),
            ports: ports.iter().copied().collect(),
            ban_duration_ms,
            geo,
            geo_provider: None,
        }
    }

    /// 注入/清除 GeoIP 数据源（对齐上游 `IPDBManager` 的装配）。
    pub fn with_provider(mut self, provider: Option<Arc<dyn GeoIpProvider>>) -> Self {
        self.geo_provider = provider;
        self
    }

    fn module(&self) -> String {
        self.config_name().to_string()
    }

    /// 对齐上游 `IPBlackList#checkIPDB`。
    ///
    /// 返回 `None` 表示「不命中，继续走下游 pass」；上游的四处 `return pass()` 一一对应。
    ///
    /// 上游该方法整体包在 `try { ... } catch (Exception e) { log.error(Lang.MODULE_IBL_EXCEPTION_GEOIP); }`
    /// 中：任何异常都只记日志并放行。Rust 侧查询不抛错——数据库缺失、IP 解析失败、
    /// 记录解码失败都统一表现为 `None`，因此无需异常分支。
    fn check_ipdb(&self, ip: &str) -> Option<CheckResult> {
        let geo = &self.geo;
        // 上游：`if (regions.isEmpty() && asns.isEmpty()) return pass();`
        // 注意这是一条整体短路——此时即便配置了城市/网络类型也不参与判定。
        if geo.regions.is_empty() && geo.asns.is_empty() {
            return None;
        }
        let address = iputil::parse_addr(ip)?;
        let data = self.geo_provider.as_ref()?.query(address)?;
        let module = self.module();

        if !geo.asns.is_empty() {
            if let Some(as_number) = data.as_data.as_ref().and_then(|data| data.number) {
                if geo.asns.contains(&as_number) {
                    // 上游 StructuredData 存的是 Long，文案参数用 String.valueOf(asn)
                    let label = as_number.to_string();
                    return Some(
                        CheckResult::ban(
                            &module,
                            self.ban_duration_ms,
                            "asn",
                            &format!("matched asn: {label}"),
                            serde_json::json!({ "type": "asn", "rule": as_number }),
                        )
                        .with_keys(
                            TranslationComponent::with_params(
                                "IP_BLACKLIST_ASN_RULE",
                                vec![label.clone().into()],
                            ),
                            TranslationComponent::with_params(
                                "MODULE_IBL_MATCH_ASN",
                                vec![label.into()],
                            ),
                        ),
                    );
                }
            }
        }

        if !geo.regions.is_empty() {
            if let Some(iso) = data
                .country
                .as_ref()
                .and_then(|country| country.iso.clone())
            {
                if geo.regions.contains(&iso) {
                    return Some(
                        CheckResult::ban(
                            &module,
                            self.ban_duration_ms,
                            "iso",
                            &format!("matched region: {iso}"),
                            serde_json::json!({ "type": "iso", "rule": iso }),
                        )
                        .with_keys(
                            TranslationComponent::with_params(
                                "IP_BLACKLIST_REGION_RULE",
                                vec![iso.clone().into()],
                            ),
                            TranslationComponent::with_params(
                                "MODULE_IBL_MATCH_REGION",
                                vec![iso.into()],
                            ),
                        ),
                    );
                }
            }
        }

        // 上游：`if (networkType != null && geoData.getNetwork() != null && geoData.getNetwork().getNetType() != null)`
        if let Some(net_type) = data
            .network
            .as_ref()
            .and_then(|network| network.net_type.clone())
        {
            if geo.network_type_hit(&net_type) {
                return Some(
                    CheckResult::ban(
                        &module,
                        self.ban_duration_ms,
                        "netType",
                        &format!("matched net type: {net_type}"),
                        serde_json::json!({ "type": "netTypes", "rule": net_type }),
                    )
                    .with_keys(
                        TranslationComponent::with_params(
                            "IP_BLACKLIST_NETTYPE_RULE",
                            vec![net_type.clone().into()],
                        ),
                        // 上游此处复用 MODULE_IBL_MATCH_IP
                        TranslationComponent::with_params(
                            "MODULE_IBL_MATCH_IP",
                            vec![net_type.into()],
                        ),
                    ),
                );
            }
        }

        if !geo.cities.is_empty() {
            if let Some(full_city_name) = data.city.as_ref().and_then(|city| city.name.as_deref()) {
                // 上游：城市名用 `contains` 匹配（"示例海南" 之类子串）
                for city in &geo.cities {
                    if full_city_name.contains(city.as_str()) {
                        return Some(
                            CheckResult::ban(
                                &module,
                                self.ban_duration_ms,
                                "city",
                                &format!("matched city: {city}"),
                                serde_json::json!({ "type": "city", "rule": city }),
                            )
                            .with_keys(
                                TranslationComponent::with_params(
                                    "IP_BLACKLIST_CITY_RULE",
                                    vec![city.clone().into()],
                                ),
                                // 上游此处复用 MODULE_IBL_MATCH_IP
                                TranslationComponent::with_params(
                                    "MODULE_IBL_MATCH_IP",
                                    vec![city.clone().into()],
                                ),
                            ),
                        );
                    }
                }
            }
        }

        None
    }
}

impl Default for IpBlacklist {
    fn default() -> Self {
        // 默认黑名单为空（用户自定义）；bypass 地址在 pipeline 层处理
        Self::new(&[], &[], 0)
    }
}

impl RuleModule for IpBlacklist {
    fn name(&self) -> &str {
        "IP Blacklist"
    }
    fn config_name(&self) -> &str {
        "ip-address-blocker"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        _torrent: &TorrentData,
        peer: &PeerData,
        _ctx: &CheckContext,
    ) -> CheckResult {
        let module = self.module();
        if peer.is_handshaking() {
            return CheckResult::handshaking(&module);
        }
        let addr = peer.address();
        if self.ports.contains(&addr.port) {
            return CheckResult::ban(
                &module,
                self.ban_duration_ms,
                "port",
                &format!("matched port: {}", addr.port),
                serde_json::json!({ "type": "port", "rule": addr.port }),
            )
            .with_keys(
                TranslationComponent::with_params(
                    "IP_BLACKLIST_PORT_RULE",
                    vec![addr.port.to_string().into()],
                ),
                TranslationComponent::with_params(
                    "MODULE_IBL_MATCH_PORT",
                    vec![addr.port.to_string().into()],
                ),
            );
        }
        if let Some(rule) = self.ips.matching(&addr.ip) {
            return CheckResult::ban(
                &module,
                self.ban_duration_ms,
                "cidr",
                &format!("matched ip/cidr: {rule}"),
                serde_json::json!({ "type": "ip", "rule": rule }),
            )
            .with_keys(
                TranslationComponent::with_params(
                    "IP_BLACKLIST_CIDR_RULE",
                    vec![rule.clone().into()],
                ),
                TranslationComponent::with_params("MODULE_IBL_MATCH_IP", vec![rule.clone().into()]),
            );
        }
        if let Some(result) = self.check_ipdb(&addr.ip) {
            return result;
        }
        CheckResult::pass(&module)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geoip::{
        AsData, CityData, CountryData, IpGeoData, NetworkData, StaticGeoIp, NET_TYPE_WIDEBAND,
    };
    use crate::module::PeerAction;
    use std::net::IpAddr;

    const PEER_IP: &str = "1.2.3.4";

    fn peer(ip: &str) -> PeerData {
        PeerData {
            client_name: Some("qBittorrent".into()),
            peer_id: Some("-qB4500-".into()),
            dl_speed: 1000,
            downloaded: 1000,
            up_speed: 1000,
            uploaded: 1000,
            progress: 0.1,
            flags: Some("d u".into()),
            ip: ip.into(),
            port: 6881,
            raw_ip: format!("{ip}:6881"),
            connection: Some("uTP".into()),
        }
    }

    /// 一个「四维度全命中」的 GeoIP 记录：ASN 64512、地区 US、城市 "示例城 示例区"、网络类型「宽带」
    fn hitting_geo_data() -> IpGeoData {
        IpGeoData {
            as_data: Some(AsData {
                number: Some(64512),
                ..AsData::default()
            }),
            country: Some(CountryData {
                name: Some("United States".into()),
                iso: Some("US".into()),
            }),
            city: Some(CityData {
                name: Some("示例城 示例区".into()),
                ..CityData::default()
            }),
            network: Some(NetworkData {
                isp: Some("示例 ISP".into()),
                net_type: Some("宽带".into()),
            }),
        }
    }

    fn provider_with(ip: &str) -> Arc<dyn GeoIpProvider> {
        let mut provider = StaticGeoIp::new();
        provider.insert(ip.parse::<IpAddr>().unwrap(), hitting_geo_data());
        Arc::new(provider)
    }

    fn all_dimensions() -> GeoIpConfig {
        GeoIpConfig::new(
            [64512],
            ["US".to_string()],
            ["示例城".to_string()],
            [NET_TYPE_WIDEBAND.to_string()],
        )
    }

    fn torrent() -> TorrentData {
        TorrentData {
            hash: "h".into(),
            name: "t".into(),
            progress: 0.0,
            total_size: 0,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: None,
        }
    }

    fn check(module: &IpBlacklist, ip: &str) -> CheckResult {
        module.check(
            "downloader",
            &torrent(),
            &peer(ip),
            &CheckContext::default(),
        )
    }

    #[test]
    fn without_geoip_database_every_dimension_misses() {
        // 四维度全配置 + 无数据库：与上游 `ipdb == null` 一致，模块只放行
        let module = IpBlacklist::with_geo(&[], &[], 0, all_dimensions());
        let result = check(&module, PEER_IP);
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.reason, "pass");
        // 每个维度单独看也都不命中
        assert!(module.check_ipdb(PEER_IP).is_none());
    }

    #[test]
    fn empty_config_lists_never_match() {
        let module = IpBlacklist::with_geo(&[], &[], 0, GeoIpConfig::default())
            .with_provider(Some(provider_with(PEER_IP)));
        assert_eq!(check(&module, PEER_IP).action, PeerAction::NoAction);
        assert!(module.check_ipdb(PEER_IP).is_none());
    }

    #[test]
    fn asn_dimension_bans_only_on_membership() {
        let geo = GeoIpConfig::new([64512], [], [], []);
        let module =
            IpBlacklist::with_geo(&[], &[], 1_000, geo).with_provider(Some(provider_with(PEER_IP)));
        let hit = module.check_ipdb(PEER_IP).expect("命中 ASN");
        assert_eq!(hit.action, PeerAction::Ban);
        assert_eq!(hit.ban_duration_ms, 1_000);
        assert_eq!(hit.rule_key.as_ref().unwrap().key, "IP_BLACKLIST_ASN_RULE");
        assert_eq!(hit.rule_key.as_ref().unwrap().params.len(), 1);
        assert_eq!(hit.reason_key.as_ref().unwrap().key, "MODULE_IBL_MATCH_ASN");
        assert_eq!(hit.data["type"], "asn");
        assert_eq!(hit.data["rule"], 64512);
        // 渲染结果逐字等于上游 UI 文案
        let translator = crate::i18n::Translator::embedded();
        assert_eq!(
            translator.render(hit.rule_key.as_ref().unwrap(), "zh_cn"),
            "ASN 规则: 64512"
        );
        assert_eq!(
            translator.render(hit.reason_key.as_ref().unwrap(), "zh_cn"),
            "匹配 ASN 规则: 64512"
        );
        assert_eq!(
            translator.render(hit.reason_key.as_ref().unwrap(), "en_us"),
            "Match ASN rule: 64512"
        );

        // 非成员：不封禁
        let other = GeoIpConfig::new([64513], [], [], []);
        let module = IpBlacklist::with_geo(&[], &[], 1_000, other)
            .with_provider(Some(provider_with(PEER_IP)));
        assert!(module.check_ipdb(PEER_IP).is_none());
    }

    #[test]
    fn region_dimension_matches_iso_code_exactly() {
        let geo = GeoIpConfig::new([], ["US".to_string()], [], []);
        let module =
            IpBlacklist::with_geo(&[], &[], 0, geo).with_provider(Some(provider_with(PEER_IP)));
        let hit = module.check_ipdb(PEER_IP).expect("命中地区");
        assert_eq!(
            hit.rule_key.as_ref().unwrap().key,
            "IP_BLACKLIST_REGION_RULE"
        );
        assert_eq!(
            hit.reason_key.as_ref().unwrap().key,
            "MODULE_IBL_MATCH_REGION"
        );
        assert_eq!(hit.data["type"], "iso");
        let translator = crate::i18n::Translator::embedded();
        assert_eq!(
            translator.render(hit.rule_key.as_ref().unwrap(), "zh_cn"),
            "国家/地区 ISO 代码规则: US"
        );
        assert_eq!(
            translator.render(hit.reason_key.as_ref().unwrap(), "en_us"),
            "Match country or city ISO code rule: US"
        );

        // 大小写敏感（上游 `regions.contains(iso)`）
        let geo = GeoIpConfig::new([], ["us".to_string()], [], []);
        let module =
            IpBlacklist::with_geo(&[], &[], 0, geo).with_provider(Some(provider_with(PEER_IP)));
        assert!(module.check_ipdb(PEER_IP).is_none());
    }

    #[test]
    fn network_type_dimension_uses_upstream_flag_map() {
        // asns 非空（但不命中）是上游整体短路的前提
        let geo = GeoIpConfig::new([999], [], [], ["wideband".to_string()]);
        let module =
            IpBlacklist::with_geo(&[], &[], 0, geo).with_provider(Some(provider_with(PEER_IP)));
        let hit = module.check_ipdb(PEER_IP).expect("命中网络类型");
        assert_eq!(
            hit.rule_key.as_ref().unwrap().key,
            "IP_BLACKLIST_NETTYPE_RULE"
        );
        // 上游网络类型分支复用 MODULE_IBL_MATCH_IP 文案键，参数是 GeoCN 的中文类型名
        assert_eq!(hit.reason_key.as_ref().unwrap().key, "MODULE_IBL_MATCH_IP");
        assert_eq!(hit.data["type"], "netTypes");
        assert_eq!(hit.data["rule"], "宽带");
        let translator = crate::i18n::Translator::embedded();
        assert_eq!(
            translator.render(hit.rule_key.as_ref().unwrap(), "zh_cn"),
            "网络类型规则"
        );
        assert_eq!(
            translator.render(hit.reason_key.as_ref().unwrap(), "zh_cn"),
            "匹配 IP 规则: 宽带"
        );

        // 只开「基站」时不命中「宽带」
        let geo = GeoIpConfig::new([999], [], [], ["baseStation".to_string()]);
        let module =
            IpBlacklist::with_geo(&[], &[], 0, geo).with_provider(Some(provider_with(PEER_IP)));
        assert!(module.check_ipdb(PEER_IP).is_none());
    }

    #[test]
    fn city_dimension_uses_contains_matching() {
        // regions 非空（但不命中）是上游整体短路的前提
        let geo = GeoIpConfig::new([], ["ZZ".to_string()], ["示例城".to_string()], []);
        let module =
            IpBlacklist::with_geo(&[], &[], 0, geo).with_provider(Some(provider_with(PEER_IP)));
        let hit = module.check_ipdb(PEER_IP).expect("命中城市");
        assert_eq!(hit.rule_key.as_ref().unwrap().key, "IP_BLACKLIST_CITY_RULE");
        // 上游城市分支同样复用 MODULE_IBL_MATCH_IP，参数是命中的配置子串
        assert_eq!(hit.reason_key.as_ref().unwrap().key, "MODULE_IBL_MATCH_IP");
        assert_eq!(hit.data["type"], "city");
        assert_eq!(hit.data["rule"], "示例城");
        let translator = crate::i18n::Translator::embedded();
        assert_eq!(
            translator.render(hit.rule_key.as_ref().unwrap(), "zh_cn"),
            "城市规则: 示例城"
        );
        assert_eq!(
            translator.render(hit.reason_key.as_ref().unwrap(), "zh_cn"),
            "匹配 IP 规则: 示例城"
        );

        let geo = GeoIpConfig::new([], ["ZZ".to_string()], ["不存在的城市".to_string()], []);
        let module =
            IpBlacklist::with_geo(&[], &[], 0, geo).with_provider(Some(provider_with(PEER_IP)));
        assert!(module.check_ipdb(PEER_IP).is_none());
    }

    /// 上游 `if (regions.isEmpty() && asns.isEmpty()) return pass();`：这是一条整体短路，
    /// 单独配置城市/网络类型在 regions 与 asns 都为空时不生效（保持与上游逐字一致）。
    #[test]
    fn geoip_block_is_short_circuited_when_asns_and_regions_are_empty() {
        let geo = GeoIpConfig::new(
            [],
            [],
            ["示例城".to_string()],
            [NET_TYPE_WIDEBAND.to_string()],
        );
        let module =
            IpBlacklist::with_geo(&[], &[], 0, geo).with_provider(Some(provider_with(PEER_IP)));
        assert!(module.check_ipdb(PEER_IP).is_none());
        assert_eq!(check(&module, PEER_IP).action, PeerAction::NoAction);
    }

    /// 数据库里没有该 IP 的记录 ⇒ 四个维度都不命中（对齐 `geoData == null -> pass`）
    #[test]
    fn unknown_ip_in_database_passes() {
        let module = IpBlacklist::with_geo(&[], &[], 0, all_dimensions())
            .with_provider(Some(provider_with("9.9.9.9")));
        assert_eq!(check(&module, PEER_IP).action, PeerAction::NoAction);
    }

    /// 端口/IP 维度不受 GeoIP 影响，仍逐字保持原有行为。
    #[test]
    fn existing_port_and_cidr_rules_still_apply() {
        let module = IpBlacklist::new(&["1.2.3.0/24".to_string()], &[6881], 0);
        let result = check(&module, PEER_IP);
        assert_eq!(result.action, PeerAction::Ban);
        assert_eq!(result.rule, "port", "端口优先级高于 CIDR");
    }
}
