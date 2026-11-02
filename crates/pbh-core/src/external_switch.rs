//! 对齐上游 `ExternalSwitch`：用环境变量 / 启动参数覆盖内部行为开关。
//!
//! Java 侧顺序为「系统属性 → 环境变量（点/横线转下划线并大写）→ 启动参数覆盖」；
//! Rust 无系统属性，等价实现为：
//! 1. 环境变量：原样键名（如 `pbh.pcb.disable-completed-excessive`）或
//!    大写下划线形式（`PBH_PCB_DISABLE_COMPLETED_EXCESSIVE`）；
//! 2. 启动参数：`--<key>=<value>` 或 `<key>=<value>` 覆盖环境变量。
//!
//! 未识别的键返回 `None`，调用方使用各自的默认值——与上游「开关不存在时走默认」一致。

/// 读取开关字符串（启动参数优先于环境变量）。
pub fn parse(key: &str) -> Option<String> {
    let env_key = key.replace(['.', '-'], "_").to_uppercase();
    let env_value = std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var(&env_key).ok().filter(|value| !value.is_empty()));
    let arg_prefix = format!("{key}=");
    let arg_prefix_dashed = format!("--{key}=");
    let arg_value = std::env::args().rev().find_map(|arg| {
        arg.strip_prefix(&arg_prefix_dashed)
            .or_else(|| arg.strip_prefix(&arg_prefix))
            .map(str::to_string)
    });
    arg_value.or(env_value)
}

/// 对齐 `ExternalSwitch.parseBoolean`：仅 `"true"`（忽略大小写）为真，缺省用 `def`。
pub fn boolean(key: &str, def: bool) -> bool {
    match parse(key) {
        Some(value) => value.trim().eq_ignore_ascii_case("true"),
        None => def,
    }
}

/// 演示模式下的打码占位（与上游 `PBHDownloaderController` 的字面量一致）。
pub const DEMO_REDACTED: &str = "REDACTED_IN_DEMO_MODE";

/// `pbh.demoMode`：演示/录屏模式。
///
/// 开启后 WebUI 展示的敏感字段会被打码：下载器地址与凭据、配置页的下载器段、
/// BTN 应用凭据（对齐上游 `PBHDownloaderController.java:323-329`、
/// `PBHGeneralController.java:348-352` 的 demoMode 分支）。
pub fn demo_mode() -> bool {
    boolean("pbh.demoMode", false)
}

/// 演示模式：把下载器 DTO 里的地址与凭据就地打码。
///
/// 键名同时覆盖 Web DTO（camelCase）与配置文件（kebab-case）两种写法，
/// 便于列表/状态/配置页共用。
pub fn redact_downloader_json(config: &mut serde_json::Value) {
    let Some(map) = config.as_object_mut() else {
        return;
    };
    for key in ["endpoint", "username", "password", "apiKey", "api-key", "token"] {
        if map.contains_key(key) {
            map.insert(key.into(), serde_json::json!(DEMO_REDACTED));
        }
    }
    for auth_key in ["basicAuth", "basic-auth"] {
        if let Some(auth) = map.get_mut(auth_key).and_then(|v| v.as_object_mut()) {
            for key in ["user", "pass"] {
                if auth.contains_key(key) {
                    auth.insert(key.into(), serde_json::json!(DEMO_REDACTED));
                }
            }
        }
    }
}

/// 对齐 `ExternalSwitch.parseInt`：非法值回退 `def`（上游会抛异常，这里取更安全方向）。
pub fn integer(key: &str, def: i64) -> i64 {
    parse(key)
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(def)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 进程环境变量是全局状态：即使用唯一键，也串行执行避免 `set_var` 撕裂。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 演示模式的打码只覆盖敏感字段，其余配置原样保留。
    #[test]
    fn redact_downloader_json_masks_credentials_only() {
        let mut value = serde_json::json!({
            "type": "qbittorrent",
            "name": "qB",
            "endpoint": "http://10.0.0.2:8080",
            "username": "admin",
            "password": "pw",
            "apiKey": "ak",
            "basicAuth": { "user": "u", "pass": "p" },
            "paused": false,
        });
        redact_downloader_json(&mut value);
        for key in ["endpoint", "username", "password", "apiKey"] {
            assert_eq!(value[key], DEMO_REDACTED, "{key} 必须打码");
        }
        assert_eq!(value["basicAuth"]["user"], DEMO_REDACTED);
        assert_eq!(value["basicAuth"]["pass"], DEMO_REDACTED);
        assert_eq!(value["name"], "qB", "非敏感字段保持原样");
        assert_eq!(value["paused"], serde_json::json!(false));

        // kebab-case（配置文件形态）同样生效
        let mut yaml_shape = serde_json::json!({
            "api-key": "ak",
            "basic-auth": { "user": "u", "pass": "p" },
            "endpoint": "http://h",
        });
        redact_downloader_json(&mut yaml_shape);
        assert_eq!(yaml_shape["api-key"], DEMO_REDACTED);
        assert_eq!(yaml_shape["basic-auth"]["user"], DEMO_REDACTED);
    }

    #[test]
    fn boolean_reads_uppercase_underscore_environment_variable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 使用唯一键，避免与并行测试的环境变量互相影响
        let key = "pbh.test.external-switch.boolean";
        std::env::set_var("PBH_TEST_EXTERNAL_SWITCH_BOOLEAN", "TRUE");
        assert!(boolean(key, false));
        std::env::set_var("PBH_TEST_EXTERNAL_SWITCH_BOOLEAN", "no");
        assert!(!boolean(key, true));
        std::env::remove_var("PBH_TEST_EXTERNAL_SWITCH_BOOLEAN");
        assert!(boolean(key, true));
    }

    #[test]
    fn integer_falls_back_on_invalid_value() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "pbh.test.external-switch.integer";
        std::env::set_var("PBH_TEST_EXTERNAL_SWITCH_INTEGER", "256");
        assert_eq!(integer(key, 128), 256);
        std::env::set_var("PBH_TEST_EXTERNAL_SWITCH_INTEGER", "not-a-number");
        assert_eq!(integer(key, 128), 128);
        std::env::remove_var("PBH_TEST_EXTERNAL_SWITCH_INTEGER");
    }
}
