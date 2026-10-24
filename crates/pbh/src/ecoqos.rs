//! Windows EcoQoS（省电优先）开关，对齐上游 `performance.windows-ecoqos-api`：
//! 通过 `SetProcessInformation(ProcessPowerThrottling)` 让本进程使用低功耗调度，
//! 代价是后台任务（ban wave 等）可能被推迟。其它平台为 no-op。

#[cfg(windows)]
pub fn apply() -> bool {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    };

    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: 1, // PROCESS_POWER_THROTTLING_CURRENT_VERSION
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    };
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ) != 0
    }
}

#[cfg(not(windows))]
pub fn apply() -> bool {
    true
}
