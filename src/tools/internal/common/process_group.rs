// process_group.rs —— 进程组管理的平台抽象。
//
// Unix: setpgid + killpg（进程组信号）
// Windows: 当前为 no-op，后续通过 Job Objects 实现（需 windows-sys crate）
//
// 的多平台进程管理。

/// 将当前进程设为新的进程组 leader。
/// 在子进程 spawn 前通过 pre_exec 调用。
pub fn become_group_leader() {
    #[cfg(unix)]
    unsafe {
        libc::setpgid(0, 0);
    }
    #[cfg(not(unix))]
    {
        // Windows: 可通过 Job Object 或 CREATE_NEW_PROCESS_GROUP 实现
    }
}

/// 终止整个进程组。
///
/// Unix: kill(-pgid, SIGTERM) → sleep(200ms) → kill(-pgid, SIGKILL)
/// Windows: 通过 TerminateJobObject 或 TerminateProcess 实现
pub fn kill_group(pgid: u32) {
    if pgid == 0 {
        return;
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGTERM);
        std::thread::sleep(std::time::Duration::from_millis(200));
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }

    #[cfg(windows)]
    {
        // TODO: 使用 Job Object 的 TerminateJobObject
        // 当前 tokio process drop 会自动清理子进程
        let _ = pgid;
    }
}

/// 在 Windows 上，通过 CREATE_NEW_PROCESS_GROUP 创建新的进程组。
/// 返回 flags 用于传递给 CreateProcess。
#[cfg(windows)]
pub const CREATE_NEW_GROUP_FLAG: u32 = 0x0000_0200; // CREATE_NEW_PROCESS_GROUP

/// 在 Windows 上授予进程组终止权限。
/// 需要在子进程创建后调用以允许父进程发送 Ctrl+C。
#[cfg(windows)]
pub fn allow_ctrl_c_for_group(pgid: u32) {
    // TODO: 需要使用 windows-sys crate
    // GenerateConsoleCtrlEvent(CTRL_C_EVENT, pgid)
    let _ = pgid;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_group_zero_is_noop() {
        // pgid 0 应该直接返回，不做任何操作
        kill_group(0);
    }
}
