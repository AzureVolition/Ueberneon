// bash_decompose.rs —— 将复合 bash 命令拆解为独立 segment。
//
// 支持拆解 &&、||、|、;、\n 等顶层操作符，同时正确跳过引号、
// 命令替换 $(...)、进程替换 <(...) / >(...)、反引号内的内容。
//
// 每个 segment 可以独立匹配权限规则，从而实现粒度控制：
//   "git status && git push --force" → ["git status", "git push --force"]
//   前段只读放行，后段触发 ForcePushGuard。

/// 将复合 bash 命令拆解为单个简单命令的 segments。
///
/// 返回 `None` 表示输入格式错误（如未闭合的引号/括号），应当回到
/// 整体字符串匹配。
/// 返回 `Some(vec)` 包含各个独立 segment，每个 segment 已被 trim。
/// 如果输入不含任何操作符，返回的 vec 只有一个元素。
pub fn decompose(command: &str) -> Option<Vec<String>> {
    let bytes = command.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return Some(vec![String::new()]);
    }

    let mut segments: Vec<String> = Vec::new();
    let mut seg_start = 0;
    let mut i = 0;

    // 解析状态
    #[allow(dead_code)]
    enum State {
        Normal,
        SingleQuote, // '...'
        DoubleQuote, // "..."
        Backtick,    // `...`
        DollarParen, // $(...)
        ProcSubst,   // <(...) 或 >(...)
        Brace,       // {...}
    }
    let mut stack: Vec<State> = Vec::new();

    // 标记当前是否在转义状态（仅 Normal / DoubleQuote 中有效）
    let mut escaped = false;

    while i < n {
        let b = bytes[i];

        if escaped {
            escaped = false;
            i += 1;
            continue;
        }

        match stack.last() {
            // ========== 不在任何嵌套结构中 ==========
            None | Some(&State::Normal) => {
                match b {
                    b'\\' => {
                        escaped = true;
                        i += 1;
                    }
                    b'\'' => {
                        stack.push(State::SingleQuote);
                        i += 1;
                    }
                    b'"' => {
                        stack.push(State::DoubleQuote);
                        i += 1;
                    }
                    b'`' => {
                        stack.push(State::Backtick);
                        i += 1;
                    }
                    b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::DollarParen);
                        i += 2; // 跳过 $(
                    }
                    b'<' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::ProcSubst);
                        i += 2; // 跳过 <(
                    }
                    b'>' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::ProcSubst);
                        i += 2; // 跳过 >(
                    }
                    b'{' => {
                        stack.push(State::Brace);
                        i += 1;
                    }
                    b';' | b'\n' => {
                        // 分割点
                        let seg = String::from_utf8_lossy(&bytes[seg_start..i])
                            .trim()
                            .to_string();
                        if !seg.is_empty() {
                            segments.push(seg);
                        }
                        seg_start = i + 1;
                        i += 1;
                    }
                    b'&' if i + 1 < n && bytes[i + 1] == b'&' => {
                        // &&
                        let seg = String::from_utf8_lossy(&bytes[seg_start..i])
                            .trim()
                            .to_string();
                        if !seg.is_empty() {
                            segments.push(seg);
                        }
                        seg_start = i + 2;
                        i += 2;
                    }
                    b'|' if i + 1 < n && bytes[i + 1] == b'|' => {
                        // ||
                        let seg = String::from_utf8_lossy(&bytes[seg_start..i])
                            .trim()
                            .to_string();
                        if !seg.is_empty() {
                            segments.push(seg);
                        }
                        seg_start = i + 2;
                        i += 2;
                    }
                    b'|' => {
                        // 单个 |（管道）
                        // 确认不是 ||——前面已经处理了
                        let seg = String::from_utf8_lossy(&bytes[seg_start..i])
                            .trim()
                            .to_string();
                        if !seg.is_empty() {
                            segments.push(seg);
                        }
                        seg_start = i + 1;
                        i += 1;
                    }
                    _ => {
                        i += 1;
                    }
                }
            }

            // ========== SingleQuote: 直到下一个 ' 为止全部字面量 ==========
            Some(&State::SingleQuote) => {
                match b {
                    b'\'' => {
                        stack.pop();
                    }
                    _ => {}
                }
                i += 1;
            }

            // ========== DoubleQuote: 转义和特殊字符 ==========
            Some(&State::DoubleQuote) => {
                match b {
                    b'\\' => {
                        escaped = true;
                    }
                    b'"' => {
                        stack.pop();
                    }
                    b'`' => {
                        // 反引号在双引号内保持命令替换语义
                        stack.push(State::Backtick);
                    }
                    b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::DollarParen);
                        i += 1; // 外层 +1，循环结尾再 +1
                    }
                    _ => {}
                }
                i += 1;
            }

            // ========== Backtick: 直到下一个 ` 为止 ==========
            Some(&State::Backtick) => {
                match b {
                    b'\\' => {
                        escaped = true;
                    }
                    b'`' => {
                        stack.pop();
                    }
                    b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::DollarParen);
                        i += 1;
                    }
                    _ => {}
                }
                i += 1;
            }

            // ========== $(...): 跟踪括号嵌套深度 ==========
            Some(&State::DollarParen) => {
                match b {
                    b'(' => {
                        stack.push(State::DollarParen);
                    }
                    b')' => {
                        stack.pop();
                    }
                    b'\'' => {
                        stack.push(State::SingleQuote);
                    }
                    b'"' => {
                        stack.push(State::DoubleQuote);
                    }
                    b'`' => {
                        stack.push(State::Backtick);
                    }
                    b'\\' => {
                        escaped = true;
                    }
                    _ => {}
                }
                i += 1;
            }

            // ========== <(...) / >(...): 跟踪括号嵌套深度 ==========
            Some(&State::ProcSubst) => {
                match b {
                    b'(' => {
                        stack.push(State::ProcSubst);
                    }
                    b')' => {
                        stack.pop();
                    }
                    b'\'' => {
                        stack.push(State::SingleQuote);
                    }
                    b'"' => {
                        stack.push(State::DoubleQuote);
                    }
                    b'`' => {
                        stack.push(State::Backtick);
                    }
                    b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::DollarParen);
                        i += 1;
                    }
                    b'\\' => {
                        escaped = true;
                    }
                    _ => {}
                }
                i += 1;
            }

            // ========== {...}: 跟踪括号嵌套深度 ==========
            Some(&State::Brace) => {
                match b {
                    b'{' => {
                        stack.push(State::Brace);
                    }
                    b'}' => {
                        stack.pop();
                    }
                    b'\'' => {
                        stack.push(State::SingleQuote);
                    }
                    b'"' => {
                        stack.push(State::DoubleQuote);
                    }
                    b'`' => {
                        stack.push(State::Backtick);
                    }
                    b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::DollarParen);
                        i += 1;
                    }
                    b'<' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::ProcSubst);
                        i += 1;
                    }
                    b'>' if i + 1 < n && bytes[i + 1] == b'(' => {
                        stack.push(State::ProcSubst);
                        i += 1;
                    }
                    b'\\' => {
                        escaped = true;
                    }
                    _ => {}
                }
                i += 1;
            }
        }
    }

    // 检查未闭合结构
    if !stack.is_empty() || escaped {
        return None;
    }

    // 最后一段
    let seg = String::from_utf8_lossy(&bytes[seg_start..])
        .trim()
        .to_string();
    if !seg.is_empty() {
        segments.push(seg);
    }

    if segments.is_empty() {
        return Some(vec![String::new()]);
    }

    Some(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 简单命令 ──

    #[test]
    fn simple_command_no_operators() {
        let result = decompose("echo hello").unwrap();
        assert_eq!(result, vec!["echo hello"]);
    }

    #[test]
    fn empty_command() {
        let result = decompose("").unwrap();
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn whitespace_only() {
        let result = decompose("   ").unwrap();
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn single_word() {
        let result = decompose("ls").unwrap();
        assert_eq!(result, vec!["ls"]);
    }

    // ── && ──

    #[test]
    fn and_chain() {
        let result = decompose("cd src && cargo build").unwrap();
        assert_eq!(result, vec!["cd src", "cargo build"]);
    }

    #[test]
    fn triple_and() {
        let result = decompose("git add . && git commit -m 'fix' && git push").unwrap();
        assert_eq!(result, vec!["git add .", "git commit -m 'fix'", "git push"]);
    }

    #[test]
    fn leading_and() {
        let result = decompose("&& echo hi").unwrap();
        assert_eq!(result, vec!["echo hi"]);
    }

    #[test]
    fn trailing_and() {
        let result = decompose("echo hi &&").unwrap();
        assert_eq!(result, vec!["echo hi"]);
    }

    // ── || ──

    #[test]
    fn or_chain() {
        let result = decompose("make || echo 'build failed'").unwrap();
        assert_eq!(result, vec!["make", "echo 'build failed'"]);
    }

    // ── | (pipe) ──

    #[test]
    fn pipe() {
        let result = decompose("cat file.txt | grep error").unwrap();
        assert_eq!(result, vec!["cat file.txt", "grep error"]);
    }

    #[test]
    fn multiple_pipes() {
        let result = decompose("cat log.txt | grep ERROR | head -5").unwrap();
        assert_eq!(result, vec!["cat log.txt", "grep ERROR", "head -5"]);
    }

    // ── ; ──

    #[test]
    fn semicolon() {
        let result = decompose("cd src; ls -la").unwrap();
        assert_eq!(result, vec!["cd src", "ls -la"]);
    }

    // ── 混合 ──

    #[test]
    fn mixed_operators() {
        let result = decompose("cd src && cargo build && cargo test; echo done").unwrap();
        assert_eq!(
            result,
            vec!["cd src", "cargo build", "cargo test", "echo done"]
        );
    }

    // ── 引号保护 ──

    #[test]
    fn inside_single_quotes() {
        let result = decompose("echo 'hello && world'").unwrap();
        assert_eq!(result, vec!["echo 'hello && world'"]);
    }

    #[test]
    fn inside_double_quotes() {
        let result = decompose("git commit -m \"fix: && || ; pipe\"").unwrap();
        assert_eq!(result, vec!["git commit -m \"fix: && || ; pipe\""]);
    }

    #[test]
    fn mixed_quotes_with_operators() {
        let result = decompose("echo 'single' && echo \"double\" | wc").unwrap();
        assert_eq!(result, vec!["echo 'single'", "echo \"double\"", "wc"]);
    }

    // ── 命令替换 ──

    #[test]
    fn dollar_paren_preserved() {
        let result = decompose("echo $(git status --porcelain) && git push").unwrap();
        assert_eq!(result, vec!["echo $(git status --porcelain)", "git push"]);
    }

    #[test]
    fn nested_dollar_paren() {
        let result = decompose("echo $(echo $(ls -la)) | head").unwrap();
        assert_eq!(result, vec!["echo $(echo $(ls -la))", "head"]);
    }

    // ── 进程替换 ──

    #[test]
    fn process_substitution() {
        let result = decompose("diff <(sort file1) <(sort file2)").unwrap();
        assert_eq!(result, vec!["diff <(sort file1) <(sort file2)"]);
    }

    // ── 反引号 ──

    #[test]
    fn backticks_preserved() {
        let result = decompose("echo `hostname` && echo hi").unwrap();
        assert_eq!(result, vec!["echo `hostname`", "echo hi"]);
    }

    // ── 错误输入 ──

    #[test]
    fn unclosed_single_quote_returns_none() {
        assert!(decompose("echo 'hello").is_none());
    }

    #[test]
    fn unclosed_dollar_paren_returns_none() {
        assert!(decompose("echo $(ls -la").is_none());
    }

    #[test]
    fn unclosed_double_quote_returns_none() {
        assert!(decompose("echo \"hello").is_none());
    }

    #[test]
    fn unclosed_backtick_returns_none() {
        assert!(decompose("echo `hostname").is_none());
    }

    #[test]
    fn trailing_backslash_returns_none() {
        assert!(decompose("echo hello\\").is_none());
    }

    // ── 真实场景 ──

    #[test]
    fn git_status_and_push() {
        let result = decompose("git status && git push --force origin main").unwrap();
        assert_eq!(result, vec!["git status", "git push --force origin main"]);
    }

    #[test]
    fn build_and_test_with_subshell() {
        let result = decompose("cd /project && cargo build 2>&1 && cargo test 2>&1").unwrap();
        assert_eq!(
            result,
            vec!["cd /project", "cargo build 2>&1", "cargo test 2>&1"]
        );
    }

    #[test]
    fn complex_with_redirect() {
        // 重定向 > file 不是操作符，不分割
        let result = decompose("echo 'start' && echo 'data' > output.txt; cat output.txt").unwrap();
        assert_eq!(
            result,
            vec!["echo 'start'", "echo 'data' > output.txt", "cat output.txt"]
        );
    }

    #[test]
    fn brace_expansion_not_split() {
        let result = decompose("echo {a,b,c}").unwrap();
        assert_eq!(result, vec!["echo {a,b,c}"]);
    }

    #[test]
    fn brace_with_operators_inside_not_split() {
        let result = decompose("mkdir -p {src,docs} && echo done").unwrap();
        assert_eq!(result, vec!["mkdir -p {src,docs}", "echo done"]);
    }

    #[test]
    fn real_world_update() {
        let cmd = "cd /home/user/project && git pull --rebase && cargo check && cargo test 2>&1 | tail -20";
        let result = decompose(cmd).unwrap();
        // | 是 shell 管道操作符，会在 2>&1 之后正确分割
        assert_eq!(
            result,
            vec![
                "cd /home/user/project",
                "git pull --rebase",
                "cargo check",
                "cargo test 2>&1",
                "tail -20",
            ]
        );
    }

    #[test]
    fn newline_as_separator() {
        let result = decompose("echo hello\necho world").unwrap();
        assert_eq!(result, vec!["echo hello", "echo world"]);
    }

    #[test]
    fn heredoc_not_special() {
        // <<EOF 不会被特殊处理，因为 < 后不是 (
        let result = decompose("cat <<EOF && echo done\nhello\nEOF").unwrap();
        // 这里 \n 会分割，包含新行的拆分
        assert!(result.len() >= 2);
    }
}
