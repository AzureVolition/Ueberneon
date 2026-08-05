// process.rs —— 进程执行器。
//
// 用 tokio::process::Command 异步执行命令，支持 timeout、
// 进程组追踪与收割、输出合并截断、沙箱隔离、自定义环境变量。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::tools::sandbox::SandboxSpec;

/// 进程执行结果。
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// 合并后的 stdout + stderr。
    pub combined: String,
    /// 进程退出码（0 表示成功）。
    pub exit_code: i32,
    /// 是否因超时被终止。
    pub timed_out: bool,
    /// 输出是否被截断（> 64KB 时头尾各保留 ~8000 字符）。
    pub truncated: bool,
}

/// 进程运行器，封装 timeout、工作目录、沙箱、环境变量等配置。
pub struct ProcessRunner {
    /// 每个命令的超时时间。
    pub timeout: Duration,
    /// 命令执行时的工作目录。
    pub work_dir: PathBuf,
    /// 沙箱隔离策略（None = 无沙箱）。
    pub sandbox: Option<SandboxSpec>,
    /// 自定义环境变量（None = 继承当前进程环境）。
    pub env: Option<HashMap<String, String>>,
    /// 取消令牌，收到取消信号时终止进程。
    pub cancel_token: Option<CancellationToken>,
}

impl ProcessRunner {
    pub fn new(work_dir: PathBuf, timeout: Duration) -> Self {
        Self {
            work_dir,
            timeout,
            sandbox: None,
            env: None,
            cancel_token: None,
        }
    }

    /// 配置取消令牌，收到取消信号时终止进程。
    pub fn with_cancel_token(mut self, token: Option<CancellationToken>) -> Self {
        self.cancel_token = token;
        self
    }

    /// 配置沙箱策略。
    pub fn with_sandbox(mut self, spec: SandboxSpec) -> Self {
        self.sandbox = Some(spec);
        self
    }

    /// 配置自定义环境变量。
    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = Some(env);
        self
    }

    /// 前台执行命令，返回合并后的 stdout+stderr。
    pub async fn run(&self, program: &str, args: &[String]) -> ProcessOutput {
        use crate::tools::sandbox::SandboxWrapped;

        // 沙箱包装
        let SandboxWrapped {
            program: prog,
            args: sandbox_args,
            profile_path,
        } = if let Some(ref spec) = self.sandbox {
            spec.wrap_command(program, args)
        } else {
            SandboxWrapped {
                program: program.to_string(),
                args: args.to_vec(),
                profile_path: None,
            }
        };

        let mut cmd = Command::new(&prog);
        cmd.args(&sandbox_args)
            .current_dir(&self.work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        // 应用自定义环境变量
        if let Some(ref env) = self.env {
            cmd.env_clear();
            for (key, value) in env {
                cmd.env(key, value);
            }
        }

        // 启用进程组
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                super::process_group::become_group_leader();
                Ok(())
            });
        }

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                Self::cleanup_profile(&profile_path);
                return ProcessOutput {
                    combined: format!("bash: failed to spawn command: {e}"),
                    exit_code: -1,
                    timed_out: false,
                    truncated: false,
                };
            }
        };

        let pid = child.id().unwrap_or(0);

        // ── 三路 select：正常完成 × 超时 × 取消 ──
        let timeout_fut = tokio::time::timeout(self.timeout, Self::wait_and_collect(child));
        tokio::pin!(timeout_fut);

        let cancel_fut = async {
            if let Some(ref token) = self.cancel_token {
                token.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(cancel_fut);

        let output = tokio::select! {
            r = &mut timeout_fut => {
                Self::kill_process_group(pid);
                match r {
                    Ok(combined) => combined,
                    Err(_elapsed) => ProcessOutput {
                        combined: format!("bash: command timed out after {}s", self.timeout.as_secs()),
                        exit_code: -1, timed_out: true, truncated: false,
                    }
                }
            }
            _ = &mut cancel_fut => {
                Self::kill_process_group(pid);
                ProcessOutput {
                    combined: "bash: command cancelled by user".into(),
                    exit_code: -1, timed_out: false, truncated: false,
                }
            }
        };

        Self::cleanup_profile(&profile_path);
        Self::maybe_truncate(output)
    }

    fn cleanup_profile(path: &Option<PathBuf>) {
        if let Some(p) = path {
            let _ = std::fs::remove_file(p);
        }
    }

    async fn wait_and_collect(mut child: tokio::process::Child) -> ProcessOutput {
        let mut stdout = child.stdout.take().expect("stdout pipe must be captured");
        let mut stderr = child.stderr.take().expect("stderr pipe must be captured");

        let mut out_buf = String::new();
        let mut err_buf = String::new();

        let stdout_fut = stdout.read_to_string(&mut out_buf);
        let stderr_fut = stderr.read_to_string(&mut err_buf);

        let (out_res, err_res) = tokio::join!(stdout_fut, stderr_fut);

        let out = if out_res.is_ok() {
            out_buf
        } else {
            String::new()
        };
        let err = if err_res.is_ok() {
            err_buf
        } else {
            String::new()
        };

        let combined = if err.is_empty() {
            out
        } else if out.is_empty() {
            err
        } else {
            format!("{out}\n{err}")
        };

        let status = child.wait().await;
        let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

        ProcessOutput {
            combined: combined.trim_end().to_string(),
            exit_code,
            timed_out: false,
            truncated: false,
        }
    }

    #[cfg(unix)]
    /// 进程是否已取消（检查 cancel_token）。
    #[allow(dead_code)]
    fn is_cancelled(token: &Option<CancellationToken>) -> bool {
        token.as_ref().map_or(false, |t| t.is_cancelled())
    }

    #[cfg(unix)]
    fn kill_process_group(pgid: u32) {
        if pgid == 0 {
            return;
        }
        unsafe {
            libc::kill(-(pgid as i32), libc::SIGTERM);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        unsafe {
            libc::kill(-(pgid as i32), libc::SIGKILL);
        }
    }

    #[cfg(not(unix))]
    fn kill_process_group(_pgid: u32) {}

    fn maybe_truncate(mut output: ProcessOutput) -> ProcessOutput {
        const MAX_LEN: usize = 64 * 1024;
        const HEAD_CHARS: usize = 8000;
        const TAIL_CHARS: usize = 8000;

        if output.combined.len() <= MAX_LEN {
            return output;
        }

        let head = &output.combined[..HEAD_CHARS.min(output.combined.len())];
        let tail_start = output.combined.len().saturating_sub(TAIL_CHARS);
        let tail = &output.combined[tail_start..];

        output.combined = format!(
            "{head}\n\n... ({total} bytes total, {kept} shown)\n\n{tail}",
            total = output.combined.len(),
            kept = HEAD_CHARS + TAIL_CHARS,
        );
        output.truncated = true;

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_simple_echo() {
        let runner = ProcessRunner::new(std::env::current_dir().unwrap(), Duration::from_secs(10));
        let out = runner.run("echo", &["hello".into()]).await;
        assert_eq!(out.combined, "hello");
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn run_nonexistent_command() {
        let runner = ProcessRunner::new(std::env::current_dir().unwrap(), Duration::from_secs(10));
        let out = runner.run("nonexistent_command_xyz", &[]).await;
        assert_ne!(out.exit_code, 0);
    }

    #[test]
    fn truncate_short_output() {
        let out = ProcessOutput {
            combined: "short".into(),
            exit_code: 0,
            timed_out: false,
            truncated: false,
        };
        let out = ProcessRunner::maybe_truncate(out);
        assert_eq!(out.combined, "short");
        assert!(!out.truncated);
    }

    #[test]
    fn truncate_long_output() {
        let long = "x".repeat(70_000);
        let out = ProcessOutput {
            combined: long,
            exit_code: 0,
            timed_out: false,
            truncated: false,
        };
        let out = ProcessRunner::maybe_truncate(out);
        assert!(out.truncated);
        assert!(out.combined.contains("bytes total"));
    }

    #[tokio::test]
    async fn run_with_custom_env() {
        let mut env = HashMap::new();
        env.insert("PATH".into(), "/usr/bin:/bin".into());
        env.insert("HOME".into(), "/tmp".into());

        let runner = ProcessRunner::new(std::env::current_dir().unwrap(), Duration::from_secs(10))
            .with_env(env);

        let out = runner.run("sh", &["-c".into(), "echo $HOME".into()]).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.combined, "/tmp");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn run_with_sandbox_enabled() {
        let spec = crate::tools::sandbox::SandboxSpec::defaults(&std::env::current_dir().unwrap());

        let runner = ProcessRunner::new(std::env::current_dir().unwrap(), Duration::from_secs(10))
            .with_sandbox(spec);

        let out = runner.run("echo", &["sandbox_works".into()]).await;
        assert_eq!(out.exit_code, 0, "sandbox failed: {}", out.combined);
        assert!(
            out.combined.contains("sandbox_works"),
            "unexpected output: {}",
            out.combined
        );
    }
}
