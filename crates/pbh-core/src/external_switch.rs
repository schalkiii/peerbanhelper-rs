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
