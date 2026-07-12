// shell.rs —— Shell 类型探测与命令构建。
//
//  自动检测 bash / sh / pwsh / powershell，
// 处理不同 shell 的 -c/-Command 差异和 chaining 兼容性。

use std::process::Command;

/// 可用的 shell 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// GNU Bash（Linux 默认、macOS 用户选择）。
    Bash,
    /// 纯 POSIX sh（fallback）。
    Sh,
    /// PowerShell Core（跨平台 `pwsh`）。
    Pwsh,
    /// Windows PowerShell（`powershell.exe`）。
    Powershell,
}

impl Shell {
    /// 探测当前系统可用的最佳 shell。
    ///
    /// 优先级：bash > pwsh > powershell（Windows）> sh
    pub fn detect() -> Self {
        // 先试探 bash
        if Self::has_command("bash") {
            return Shell::Bash;
        }

        // Windows：优先 pwsh，其次 powershell
        #[cfg(target_os = "windows")]
        {
            if Self::has_command("pwsh") {
                return Shell::Pwsh;
            }
            if Self::has_command("powershell") {
                return Shell::Powershell;
            }
        }

        // 最后 fallback 到 sh
        Shell::Sh
    }

    /// 将用户脚本包装为可供进程执行的 argv。
    ///
    /// 返回 `(程序路径, argv 列表)`。
    pub fn build_command(&self, script: &str) -> (String, Vec<String>) {
        match self {
            Shell::Bash => ("bash".into(), vec!["-c".into(), script.to_string()]),
            Shell::Sh => ("sh".into(), vec!["-c".into(), script.to_string()]),
            Shell::Pwsh => ("pwsh".into(), vec!["-NoProfile".into(), "-Command".into(), script.to_string()]),
            Shell::Powershell => ("powershell.exe".into(), vec!["-NoProfile".into(), "-Command".into(), script.to_string()]),
        }
    }

    /// 是否支持命令链式调用（`&&`、`||`、`;` 等）。
    ///
    /// PowerShell 的 `&&` 语法仅在 v7+ 支持，此处保守返回 false。
    pub fn supports_chaining(&self) -> bool {
        matches!(self, Shell::Bash | Shell::Sh)
    }

    /// 检查某个命令是否在 PATH 中可用。
    fn has_command(name: &str) -> bool {
        Command::new(if cfg!(target_os = "windows") { "where" } else { "which" })
            .arg(name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_shell() {
        let shell = Shell::detect();
        // 在任何类 Unix 系统上，至少能找到 sh
        // 大多数 CI / macOS 能探测到 bash
        assert!(matches!(shell, Shell::Bash | Shell::Sh | Shell::Pwsh | Shell::Powershell));
    }

    #[test]
    fn build_bash_command() {
        let (prog, argv) = Shell::Bash.build_command("echo hello");
        assert_eq!(prog, "bash");
        assert_eq!(argv, vec!["-c", "echo hello"]);
    }

    #[test]
    fn build_sh_command() {
        let (prog, argv) = Shell::Sh.build_command("echo hello");
        assert_eq!(prog, "sh");
        assert_eq!(argv, vec!["-c", "echo hello"]);
    }

    #[test]
    fn build_pwsh_command() {
        let (prog, argv) = Shell::Pwsh.build_command("Write-Host hello");
        assert_eq!(prog, "pwsh");
        assert!(argv.contains(&"-NoProfile".into()));
        assert!(argv.contains(&"-Command".into()));
        assert!(argv.contains(&"Write-Host hello".into()));
    }

    #[test]
    fn bash_supports_chaining() {
        assert!(Shell::Bash.supports_chaining());
        assert!(Shell::Sh.supports_chaining());
    }

    #[test]
    fn powershell_no_chaining() {
        assert!(!Shell::Pwsh.supports_chaining());
        assert!(!Shell::Powershell.supports_chaining());
    }
}
