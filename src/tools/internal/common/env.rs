// env.rs —— 子进程环境变量处理。
//
// - PATH 合并：从登录 shell 探测 $PATH，与当前进程 PATH 合并
// - Secrets 过滤：从子进程环境中移除 API key 等敏感变量
// - 环境继承：继承当前进程环境，合并探测到的 PATH

use std::collections::HashMap;
use std::process::Command;

/// 已知的 secrets 环境变量前缀 / 名称模式。
const SECRET_PATTERNS: &[&str] = &[
    // API keys
    "API_KEY",
    "API_SECRET",
    "SECRET_KEY",
    "PRIVATE_KEY",
    "ACCESS_KEY",
    "SECRET_ACCESS_KEY",
    // Tokens
    "TOKEN",
    "ACCESS_TOKEN",
    "REFRESH_TOKEN",
    "AUTH_TOKEN",
    "SESSION_TOKEN",
    "PERSONAL_ACCESS_TOKEN",
    // Passwords
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    // Provider-specific
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "DEEPSEEK_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AZURE_CLIENT_SECRET",
    "GITHUB_TOKEN",
    "NPM_TOKEN",
    "DOCKER_PASSWORD",
    // Generic secrets
    "SECRET",
    "PRIVATE",
    "ENCRYPTION_KEY",
    "SIGNING_KEY",
];

/// 为子进程构建安全的环境变量。
///
/// - 继承当前进程的全部环境变量
/// - 合并从登录 shell 探测到的 PATH（确保命令能正确解析）
/// - 过滤掉已知的 secrets 变量（替换为 `[redacted]`）
pub struct EnvBuilder {
    /// 合并后的环境变量。
    vars: HashMap<String, String>,
}

impl EnvBuilder {
    /// 创建新的环境构建器，继承当前进程环境。
    pub fn inherit() -> Self {
        let vars: HashMap<String, String> = std::env::vars().collect();
        Self { vars }
    }

    /// 探测登录 shell 的 PATH 并合并。
    ///
    ///  从 `bash -l -c 'echo $PATH'`（或适合当前 shell 的命令）
    /// 获取登录 shell 的 PATH，与当前进程的 PATH 合并，
    /// 确保从 GUI 启动的 agent 也能找到 Homebrew 等包管理器的命令。
    pub fn merge_login_path(&mut self) {
        if let Some(login_path) = Self::probe_login_path() {
            let current_path = self.vars.get("PATH").cloned().unwrap_or_default();

            let merged = Self::merge_paths(&current_path, &login_path);
            self.vars.insert("PATH".into(), merged);
        }
    }

    /// 过滤已知的 secrets 变量。
    ///
    /// secrets 变量不会被删除，而是替换为 `[redacted]`，
    /// 确保脚本依赖这些变量名时不会因变量缺失而报错。
    pub fn filter_secrets(&mut self) {
        let keys: Vec<String> = self.vars.keys().cloned().collect();
        for key in keys {
            if Self::is_secret(&key) {
                self.vars.insert(key, "[redacted]".into());
            }
        }
    }

    /// 消费构建器，返回 `HashMap<String, String>`。
    pub fn build(self) -> HashMap<String, String> {
        self.vars
    }

    /// 将环境变量应用到 tokio::process::Command。
    pub fn apply_to_command(&self, cmd: &mut tokio::process::Command) {
        // 清除默认环境，然后逐个设置
        cmd.env_clear();
        for (key, value) in &self.vars {
            cmd.env(key, value);
        }
    }

    // ── 内部实现 ────────────────────────────────────────────────────────

    /// 探测登录 shell 的 PATH。
    fn probe_login_path() -> Option<String> {
        // 优先用 bash login shell
        let shell = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
            "bash"
        } else {
            "sh"
        };

        let output = Command::new(shell)
            .args(["-l", "-c", "echo $PATH"])
            .env_clear()
            // 继承最小环境以让 bash 能启动
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin")
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;

        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }

        None
    }

    /// 合并两个 PATH 字符串，去重并保持顺序。
    fn merge_paths(current: &str, login: &str) -> String {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        // 先加入 login PATH（优先级高）
        for part in login.split(':') {
            let trimmed = part.trim();
            if !trimmed.is_empty() && seen.insert(trimmed) {
                result.push(trimmed.to_string());
            }
        }

        // 再加入当前 PATH
        for part in current.split(':') {
            let trimmed = part.trim();
            if !trimmed.is_empty() && seen.insert(trimmed) {
                result.push(trimmed.to_string());
            }
        }

        result.join(":")
    }

    /// 判断变量名是否匹配 secrets 模式。
    fn is_secret(name: &str) -> bool {
        let upper = name.to_uppercase();
        SECRET_PATTERNS.iter().any(|pat| {
            // 精确匹配或包含模式
            upper == *pat
                || upper.starts_with(pat)
                || upper.ends_with(pat)
                || upper.contains(&format!("_{pat}_"))
                || upper.contains(&format!("_{pat}"))
                || upper.contains(&format!("{pat}_"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_paths_dedup() {
        let current = "/usr/bin:/usr/local/bin";
        let login = "/opt/homebrew/bin:/usr/local/bin:/usr/bin";
        let merged = EnvBuilder::merge_paths(current, login);
        // /opt/homebrew/bin 应排在最前面
        assert!(merged.starts_with("/opt/homebrew/bin"));
        // 不重复
        assert_eq!(merged.matches("/usr/bin").count(), 1);
    }

    #[test]
    fn is_secret_matches_api_key() {
        assert!(EnvBuilder::is_secret("OPENAI_API_KEY"));
        assert!(EnvBuilder::is_secret("ANTHROPIC_API_KEY"));
        assert!(EnvBuilder::is_secret("MY_API_KEY"));
    }

    #[test]
    fn is_secret_matches_token() {
        assert!(EnvBuilder::is_secret("GITHUB_TOKEN"));
        assert!(EnvBuilder::is_secret("ACCESS_TOKEN"));
    }

    #[test]
    fn is_secret_matches_password() {
        assert!(EnvBuilder::is_secret("DB_PASSWORD"));
        assert!(EnvBuilder::is_secret("MY_PASSWD"));
    }

    #[test]
    fn is_secret_ignores_safe_vars() {
        assert!(!EnvBuilder::is_secret("HOME"));
        assert!(!EnvBuilder::is_secret("PATH"));
        assert!(!EnvBuilder::is_secret("USER"));
        assert!(!EnvBuilder::is_secret("LANG"));
        assert!(!EnvBuilder::is_secret("EDITOR"));
    }

    #[test]
    fn filter_secrets_redacts_values() {
        let mut builder = EnvBuilder::inherit();
        // 设置一个假 secret
        builder.vars.insert("MY_API_KEY".into(), "sk-abc123".into());
        builder.vars.insert("HOME".into(), "/home/user".into());

        builder.filter_secrets();

        assert_eq!(builder.vars.get("MY_API_KEY").unwrap(), "[redacted]");
        assert_eq!(builder.vars.get("HOME").unwrap(), "/home/user");
    }

    #[test]
    fn probe_login_path_returns_something() {
        let path = EnvBuilder::probe_login_path();
        // 在 CI/macOS 上应该能探测到 PATH
        if let Some(p) = path {
            assert!(!p.is_empty());
            assert!(p.contains("/"));
        }
    }
}
