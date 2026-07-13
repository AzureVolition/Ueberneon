// sandbox/mod.rs —— 操作系统级命令隔离。
//
// 对 macOS / Linux / Windows 分别提供
// 沙箱包装策略。启用与否由消费者通过 Option<SandboxSpec> 控制：
// - None  = 禁用沙箱，直接透传
// - Some  = 启用沙箱，wrap_command 始终包装
//
// ┌──────────┬──────────────────────────────────┐
// │ 平台      │ 沙箱机制                          │
// ├──────────┼──────────────────────────────────┤
// │ macOS    │ sandbox-exec + Seatbelt profile  │
// │ Linux    │ bwrap (bubblewrap)               │
// │ Windows  │ Job Object（注1）                 │
// └──────────┴──────────────────────────────────┘
//
// 注1：Windows 没有等同于 sandbox-exec / bwrap 的内置命令行沙箱。
//      此处通过 Job Object 提供进程组隔离；
//      完整沙箱（文件系统/网络限制）需使用 AppContainer API 或 Windows Sandbox，
//      属于后续迭代内容。

use std::path::{Path, PathBuf};

/// 沙箱隔离策略。
///
/// 纯策略描述，不包含启用/禁用开关。
/// 消费者（Bash / ProcessRunner）通过 `Option<SandboxSpec>` 控制：
/// - `None`  → 禁用沙箱，命令直接透传
/// - `Some(spec)` → 启用沙箱，`wrap_command` 总是执行包装
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// 允许写入的路径列表。
    pub write_roots: Vec<PathBuf>,
    /// 只读挂载的根路径（macOS/Linux 为 "/"，Windows 为 "C:\"）。
    pub read_only_roots: Vec<PathBuf>,
    /// 是否允许网络访问。
    pub allow_network: bool,
}

/// `wrap_command` 的返回结果。
pub struct SandboxWrapped {
    /// 要执行的程序。
    pub program: String,
    /// 参数列表。
    pub args: Vec<String>,
    /// 沙箱 profile 临时文件路径（macOS 专用），需在命令退出后清理。
    pub profile_path: Option<PathBuf>,
}

impl SandboxSpec {
    /// 根据当前平台创建默认沙箱配置。
    ///
    /// 返回一个安全的默认策略：
    /// - 只写 work_dir + 临时目录
    /// - 只读系统根
    /// - 禁止网络
    pub fn defaults(work_dir: &Path) -> Self {
        let roots = if cfg!(windows) {
            vec![PathBuf::from("C:\\")]
        } else {
            vec![PathBuf::from("/")]
        };

        Self {
            write_roots: vec![work_dir.to_path_buf(), std::env::temp_dir()],
            read_only_roots: roots,
            allow_network: false,
        }
    }

    /// 将 argv 包装为沙箱命令。
    ///
    /// 注意：仅当你有 `&SandboxSpec` 时调用此方法 ——
    /// 即消费者已经决定启用沙箱。如果要禁用，消费者应使用 `None`。
    pub fn wrap_command(&self, program: &str, args: &[String]) -> SandboxWrapped {
        let inner_cmd = Self::join_command(program, args);

        #[cfg(target_os = "macos")]
        { self.wrap_macos(&inner_cmd) }

        #[cfg(target_os = "linux")]
        { self.wrap_linux(&inner_cmd) }

        #[cfg(target_os = "windows")]
        { self.wrap_windows(&inner_cmd) }
    }

    fn join_command(program: &str, args: &[String]) -> String {
        let mut parts = vec![program.to_string()];
        parts.extend(args.iter().cloned());
        parts.join(" ")
    }

    // ── macOS: sandbox-exec + Seatbelt profile ────────────────────────────

    #[cfg(target_os = "macos")]
    fn wrap_macos(&self, inner_cmd: &str) -> SandboxWrapped {
        let profile = self.build_macos_profile();
        let profile_path = std::env::temp_dir().join(format!(
            "racp_sandbox_{}.sb",
            std::process::id()
        ));
        if let Err(e) = std::fs::write(&profile_path, &profile) {
            tracing::warn!(
                "sandbox: failed to write macOS profile to {}: {e}",
                profile_path.display()
            );
            return SandboxWrapped {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), inner_cmd.to_string()],
                profile_path: None,
            };
        }

        let argv = vec![
            "-f".to_string(),
            profile_path.display().to_string(),
            "sh".to_string(),
            "-c".to_string(),
            inner_cmd.to_string(),
        ];

        SandboxWrapped {
            program: "sandbox-exec".to_string(),
            args: argv,
            profile_path: Some(profile_path),
        }
    }

    #[cfg(target_os = "macos")]
    fn build_macos_profile(&self) -> String {
        // TODO: 从 (allow default) 逐步收紧为 (deny default) + 精确权限
        let mut profile = String::from("(version 1)\n");
        profile.push_str("(allow default)\n");
        profile
    }

    // ── Linux: bwrap (bubblewrap) ─────────────────────────────────────────

    #[cfg(target_os = "linux")]
    fn wrap_linux(&self, inner_cmd: &str) -> SandboxWrapped {
        let mut argv = vec![
            "bwrap".to_string(),
            "--unshare-all".to_string(),
            "--die-with-parent".to_string(),
        ];

        for root in &self.read_only_roots {
            argv.push("--ro-bind".to_string());
            argv.push(root.display().to_string());
            argv.push(root.display().to_string());
        }

        for path in &self.write_roots {
            argv.push("--bind".to_string());
            argv.push(path.display().to_string());
            argv.push(path.display().to_string());
        }

        argv.extend_from_slice(&[
            "--dev".to_string(), "/dev".to_string(),
            "--proc".to_string(), "/proc".to_string(),
            "--tmpfs".to_string(), "/tmp".to_string(),
        ]);

        if !self.allow_network {
            argv.push("--unshare-net".to_string());
        }

        argv.push("--".to_string());
        argv.push("sh".to_string());
        argv.push("-c".to_string());
        argv.push(inner_cmd.to_string());

        SandboxWrapped {
            program: "bwrap".to_string(),
            args: argv,
            profile_path: None,
        }
    }

    // ── Windows: Job Object 进程组隔离 ────────────────────────────────────

    /// Windows 没有内置的命令行沙箱（sandbox-exec / bwrap）。
    ///
    /// 当前策略：
    /// - **进程组隔离**：由 `ProcessRunner` 通过 `CREATE_BREAKAWAY_FROM_JOB`
    ///   或 Job Object 确保子进程组可被统一终止。
    /// - **完整沙箱**（文件系统/网络限制）：需要 AppContainer Profile API
    ///   或 Windows Sandbox。这些是未来迭代内容。
    ///
    /// 此处不做额外包装，直接透传原始命令。
    #[cfg(target_os = "windows")]
    fn wrap_windows(&self, inner_cmd: &str) -> SandboxWrapped {
        // Windows 上 inner_cmd 已经由 Shell::build_command() 包装为
        // cmd.exe /c "..." 或 powershell.exe -Command "..."
        // 直接作为 program + args 透传即可。
        //
        // 未来如果加入 AppContainer 支持，此处会改为：
        //   1. 通过 CreateAppContainerProfile 创建容器
        //   2. 用 PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES 启动进程
        let parts: Vec<&str> = inner_cmd.splitn(2, ' ').collect();
        if parts.len() == 2 {
            SandboxWrapped {
                program: parts[0].to_string(),
                args: vec![parts[1].to_string()],
                profile_path: None,
            }
        } else {
            SandboxWrapped {
                program: inner_cmd.to_string(),
                args: vec![],
                profile_path: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_has_work_dir() {
        let spec = SandboxSpec::defaults(Path::new("/tmp/work"));
        assert!(spec.write_roots.contains(&PathBuf::from("/tmp/work")));
    }

    #[test]
    fn defaults_platform_root() {
        let spec = SandboxSpec::defaults(Path::new("/tmp/work"));
        let expected = if cfg!(windows) { "C:\\" } else { "/" };
        assert_eq!(spec.read_only_roots[0].to_str().unwrap(), expected);
    }

    #[test]
    fn join_command_simple() {
        assert_eq!(
            SandboxSpec::join_command("bash", &["-c".into(), "echo hi".into()]),
            "bash -c echo hi"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_wrap_uses_sandbox_exec() {
        let spec = SandboxSpec::defaults(Path::new("/tmp/work"));
        let w = spec.wrap_command("bash", &["-c".into(), "echo hi".into()]);
        assert_eq!(w.program, "sandbox-exec");
        assert!(w.profile_path.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_uses_bwrap() {
        let spec = SandboxSpec::defaults(Path::new("/tmp/work"));
        let w = spec.wrap_command("echo", &["hi".into()]);
        assert_eq!(w.program, "bwrap");
    }
}
