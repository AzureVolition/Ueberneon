// edit 模块 —— 字符串编辑引擎（精确匹配 → CRLF 归一化 → 模糊匹配）。
//
// 用作 edit_file / multi_edit 的底层替换逻辑。

/// 一次编辑操作的结果。
#[derive(Debug, Clone)]
pub struct EditResult {
    /// 修改后的完整内容。
    pub updated: String,
    /// 实际执行的替换次数。
    pub applied: usize,
    /// 内容中匹配 old_string 的总次数。
    pub matches: usize,
    /// 是否通过模糊匹配找到。
    pub fuzzy: bool,
}

/// 对内容执行替换。
///
/// 匹配策略（两级）：
/// 1. **精确匹配**：计数并检查唯一性
/// 2. **CRLF 归一化**：若内容使用 `\r\n` 但 old_string 使用 `\n`，自动转换后重试
/// 3. **模糊匹配**：剥离 read_file 行号前缀（`  42→`）→ trim 尾部空白 → 展开 tab
pub fn apply_edit(content: &str, old_string: &str, new_string: &str, replace_all: bool) -> EditResult {
    // —— 第一关：精确匹配 ——
    let exact_old = normalize_line_endings_for_match(content, old_string);
    let exact_matches = count_occurrences(content, &exact_old);

    if exact_matches == 1 {
        let updated = if replace_all {
            content.replace(&exact_old, new_string)
        } else {
            content.replacen(&exact_old, new_string, 1)
        };
        return EditResult {
            updated,
            applied: 1,
            matches: exact_matches,
            fuzzy: false,
        };
    }
    if exact_matches > 1 && !replace_all {
        return EditResult {
            updated: content.to_string(),
            applied: 0,
            matches: exact_matches,
            fuzzy: false,
        };
    }
    if exact_matches > 1 && replace_all {
        let updated = content.replace(&exact_old, new_string);
        return EditResult {
            updated,
            applied: exact_matches,
            matches: exact_matches,
            fuzzy: false,
        };
    }

    // —— 第二关：CRLF 归一化 ——
    let crlf_old = normalize_crlf(old_string);
    if crlf_old != old_string {
        return apply_fuzzy_fallback(content, &crlf_old, new_string, replace_all);
    }

    // —— 第三关：模糊匹配 ——
    apply_fuzzy_fallback(content, old_string, new_string, replace_all)
}

/// 对内容分词匹配。处理 blank 行、读文件前缀剥离、trim、tab 展开。
fn apply_fuzzy_fallback(content: &str, old_string: &str, new_string: &str, replace_all: bool) -> EditResult {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<String> = normalize_lines_for_fuzzy(old_string);

    if old_lines.is_empty() || content_lines.is_empty() {
        return EditResult {
            updated: content.to_string(),
            applied: 0,
            matches: 0,
            fuzzy: true,
        };
    }

    // 收集所有可能的匹配区间
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + old_lines.len() <= content_lines.len() {
        let window = &content_lines[i..i + old_lines.len()];
        if fuzzy_match_window(window, &old_lines) {
            ranges.push((i, i + old_lines.len()));
            i += if replace_all { old_lines.len() } else { 1 };
            if !replace_all && ranges.len() > 1 {
                break;
            }
        } else {
            i += 1;
        }
    }

    if ranges.is_empty() {
        return EditResult {
            updated: content.to_string(),
            applied: 0,
            matches: 0,
            fuzzy: true,
        };
    }

    if ranges.len() == 1 || replace_all {
        let _applied = ranges.len();
        let new_lines: Vec<&str> = new_string.lines().collect();
        let mut result_lines: Vec<&str> = Vec::new();
        let mut last_end = 0;

        for (start, end) in &ranges {
            result_lines.extend_from_slice(&content_lines[last_end..*start]);
            result_lines.extend_from_slice(&new_lines);
            last_end = *end;
        }
        result_lines.extend_from_slice(&content_lines[last_end..]);

        let updated = if content.ends_with('\n') {
            let mut s = result_lines.join("\n");
            s.push('\n');
            s
        } else {
            result_lines.join("\n")
        };

        return EditResult {
            updated,
            applied: ranges.len(),
            matches: ranges.len(),
            fuzzy: true,
        };
    }

    // >1 匹配且非 replace_all
    EditResult {
        updated: content.to_string(),
        applied: 0,
        matches: ranges.len(),
        fuzzy: true,
    }
}

/// 对于内容使用 CRLF 行结尾的情形，将 old_string 也归一化为 CRLF 再进行匹配。
/// 如果 old_string 本身已经是 CRLF 或内容不使用 CRLF，返回原字符串。
fn normalize_line_endings_for_match(content: &str, old_string: &str) -> String {
    if content.contains("\r\n") && !old_string.contains("\r\n") && old_string.contains('\n') {
        old_string.replace('\n', "\r\n")
    } else {
        old_string.to_string()
    }
}

/// 将 old_string 的 LF 转为 CRLF。
fn normalize_crlf(s: &str) -> String {
    if !s.contains("\r\n") && s.contains('\n') {
        s.replace('\n', "\r\n")
    } else {
        s.to_string()
    }
}

/// 计算子串出现次数（不重叠）。
fn count_occurrences(content: &str, pattern: &str) -> usize {
    if pattern.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = content[start..].find(pattern) {
        count += 1;
        start += pos + pattern.len();
    }
    count
}

/// 将 old_string 中每行归一化用于模糊匹配：
/// 剥离读文件行号前缀 → trim 尾部空白 → 展开 tab
fn normalize_lines_for_fuzzy(s: &str) -> Vec<String> {
    s.lines()
        .map(|line| {
            let stripped = strip_read_file_prefix(line);
            let trimmed = strip_trailing_whitespace(&stripped);
            expand_tabs(&trimmed, 4)
        })
        .collect()
}

/// 剥离读文件行号前缀：`  42→content` → `content`
/// 支持可变数量的前导空格/制表符 + 数字 + → (U+2192)
fn strip_read_file_prefix(line: &str) -> String {
    let trimmed = line.trim_start();
    // 检查是否以数字开头（经过 trim_start 后）
    if let Some(rest) = trimmed.strip_suffix('→') {
        // 剩下的应该是纯数字
        if rest.chars().all(|c| c.is_ascii_digit()) {
            return String::new(); // 整行都是前缀，可能没有实际内容
        }
    }
    // 查找 → 符号
    if let Some(pos) = trimmed.find('→') {
        let prefix = &trimmed[..pos];
        if prefix.chars().all(|c| c.is_ascii_digit() || c.is_whitespace()) {
            return trimmed[pos + '→'.len_utf8()..].to_string();
        }
    }
    line.to_string()
}

fn strip_trailing_whitespace(s: &str) -> String {
    s.trim_end().to_string()
}

fn expand_tabs(s: &str, tab_size: usize) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\t' {
            // 补齐到下一个 tab stop
            let col = result.len() % tab_size;
            let spaces = tab_size - col;
            for _ in 0..spaces {
                result.push(' ');
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// 检查一行是否匹配单行 old_string 的任一种归一化形式。
fn line_matches_fuzzy(content_line: &str, old_normalized: &str) -> bool {
    // 尝试多种归一化方式匹配
    let candidates = [
        // 完全原始
        content_line,
        // 去掉 read_file 前缀
        &strip_read_file_prefix(content_line),
    ];

    for candidate in &candidates {
        let trimmed = strip_trailing_whitespace(candidate);
        let expanded = expand_tabs(&trimmed, 4);
        if expanded == *old_normalized {
            return true;
        }
    }
    false
}

/// 对一行内容应用多种归一化模式，检查是否与 old_lines 窗口匹配。
fn fuzzy_match_window(content_window: &[&str], old_normalized: &[String]) -> bool {
    if content_window.len() != old_normalized.len() {
        return false;
    }
    for (c, o) in content_window.iter().zip(old_normalized.iter()) {
        if !line_matches_fuzzy(c, o) {
            return false;
        }
    }
    true
}

// ── 错误消息辅助 ─────────────────────────────────────────────────────────────

/// 构建 old_string 未找到的错误消息，包含最接近的行建议。
pub fn old_string_not_found_error(path: &str, old_string: &str, content: &str) -> String {
    let hint = find_nearest_line(old_string, content);
    let mut msg = format!("old_string not found in {}", path);
    if let Some((line, text)) = hint {
        msg.push_str(&format!(" (nearest line {}: {:?})", line, text));
    }
    msg.push_str(
        ";\n  re-read the file to confirm its current content before retrying the edit",
    );
    msg
}

/// 构建 old_string 不唯一的错误消息，列出匹配行。
pub fn old_string_not_unique_error(path: &str, old_string: &str, content: &str, matches: usize) -> String {
    let lines = match_line_summary(old_string, content, 5);
    format!(
        "old_string is not unique in {} ({} matches){};\n  add nearby unique code, not just repeated separator lines",
        path, matches, lines
    )
}

/// 在内容中搜索与 old_string 最接近的行，返回 (行号, 行内容)。
fn find_nearest_line(old_string: &str, content: &str) -> Option<(usize, String)> {
    let old_trimmed = old_string.trim();
    if old_trimmed.is_empty() {
        return None;
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut best_score: isize = -1;
    let mut best: Option<(usize, String)> = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let score = common_prefix_score(old_trimmed, trimmed);
        if score > best_score {
            best_score = score;
            best = Some((i + 1, trimmed.to_string()));
        }
    }

    best
}

/// 计算两个字符串的共同前缀长度（逐字符，大小写敏感）。
fn common_prefix_score(a: &str, b: &str) -> isize {
    a.chars()
        .zip(b.chars())
        .take_while(|(ac, bc)| ac == bc)
        .count() as isize
}

/// 列出 old_string 在内容中的前 N 个匹配行号。
fn match_line_summary(old_string: &str, content: &str, max_lines: usize) -> String {
    let mut lines = Vec::new();
    let mut pos = 0;
    let _content_bytes = content.as_bytes();
    let old_bytes = old_string.as_bytes();

    while let Some(found) = content[pos..].find(old_string) {
        let abs_pos = pos + found;
        // 计算行号
        let line_num = content[..abs_pos].chars().filter(|&c| c == '\n').count() + 1;
        lines.push(line_num);
        if lines.len() >= max_lines {
            break;
        }
        pos = abs_pos + old_bytes.len();
    }

    if lines.is_empty() {
        return String::new();
    }

    let line_strs: Vec<String> = lines.iter().map(|n| format!("line {}", n)).collect();
    format!(" (matching: {})", line_strs.join(", "))
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_unique() {
        let content = "hello\nworld\nhello\n";
        let result = apply_edit(content, "world", "there", false);
        assert_eq!(result.applied, 1);
        assert_eq!(result.matches, 1);
        assert!(!result.fuzzy);
        assert_eq!(result.updated, "hello\nthere\nhello\n");
    }

    #[test]
    fn exact_match_not_unique() {
        let content = "hello\nworld\nhello\n";
        let result = apply_edit(content, "hello", "hi", false);
        assert_eq!(result.applied, 0);
        assert_eq!(result.matches, 2);
    }

    #[test]
    fn exact_match_not_found() {
        let content = "hello\nworld\n";
        let result = apply_edit(content, "xyz", "abc", false);
        assert_eq!(result.applied, 0);
        assert_eq!(result.matches, 0);
    }

    #[test]
    fn replace_all() {
        let content = "a\nb\na\nb\na\n";
        let result = apply_edit(content, "a", "x", true);
        assert_eq!(result.applied, 3);
        assert_eq!(result.matches, 3);
        assert_eq!(result.updated, "x\nb\nx\nb\nx\n");
    }

    #[test]
    fn delete_string() {
        let content = "hello world\n";
        let result = apply_edit(content, "world", "", false);
        assert_eq!(result.applied, 1);
        assert_eq!(result.updated, "hello \n");
    }

    #[test]
    fn crlf_normalization() {
        let content = "line1\r\nline2\r\nline3\r\n";
        let result = apply_edit(content, "line2", "changed", false);
        assert_eq!(result.applied, 1, "exact CRLF match should work");
        assert_eq!(result.updated, "line1\r\nchanged\r\nline3\r\n");
    }

    #[test]
    fn fuzzy_read_file_prefix() {
        // old_string 包含 read_file 的行号前缀（如从 read_file 输出复制），
        // 但实际文件内容没有此前缀。模糊匹配应剥离此前缀。
        let content = "hello world\nfoo bar\n";
        let result = apply_edit(content, "  42→hello world", "hi there", false);
        assert_eq!(result.applied, 1);
        assert!(result.fuzzy);
        assert!(result.updated.contains("hi there"));
    }

    #[test]
    fn fuzzy_trailing_whitespace() {
        // old_string 末尾没有空格，但内容行末尾有空格。
        // "hello world\n" 不在 "hello world   \n" 中。
        let content = "hello world   \nfoo bar\n";
        let result = apply_edit(content, "hello world\n", "hi\n", false);
        assert_eq!(result.applied, 1);
        assert!(result.fuzzy);
        assert!(result.updated.contains("hi\nfoo"));
    }

    #[test]
    fn count_occurrences_basic() {
        assert_eq!(count_occurrences("abcabc", "abc"), 2);
        assert_eq!(count_occurrences("aaaa", "aa"), 2); // 不重叠
        assert_eq!(count_occurrences("hello", "x"), 0);
        assert_eq!(count_occurrences("", "x"), 0);
    }

    #[test]
    fn strip_read_file_prefix_basic() {
        assert_eq!(strip_read_file_prefix("  42→hello"), "hello");
        assert_eq!(strip_read_file_prefix("1→foo"), "foo");
        assert_eq!(strip_read_file_prefix("hello"), "hello");
        assert_eq!(strip_read_file_prefix("  42→"), "");
    }

    #[test]
    fn expand_tabs_basic() {
        assert_eq!(expand_tabs("\thello", 4), "    hello");
        assert_eq!(expand_tabs("a\tb", 4), "a   b");
        assert_eq!(expand_tabs("no tabs", 4), "no tabs");
    }

    #[test]
    fn old_string_not_found_error_basic() {
        let content = "first line\nsecond line\nthird line\n";
        let msg = old_string_not_found_error("test.txt", "second line", content);
        // 应该能找到 second line
        assert!(msg.contains("nearest line 2"), "msg: {}", msg);
    }

    #[test]
    fn match_line_summary_basic() {
        let content = "a\nb\na\nb\na\n";
        let summary = match_line_summary("a", content, 3);
        assert!(summary.contains("line 1"), "summary: {}", summary);
        assert!(summary.contains("line 3"), "summary: {}", summary);
        assert!(summary.contains("line 5"), "summary: {}", summary);
    }

    #[test]
    fn normalize_lines_for_fuzzy_strips_prefix() {
        let lines = normalize_lines_for_fuzzy("  42→hello world\n  43→foo bar\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello world");
        assert_eq!(lines[1], "foo bar");
    }

    #[test]
    fn fuzzy_match_empty_lines() {
        let content = "\n\n\n";
        let result = apply_edit(content, "non-existent", "replacement", false);
        assert_eq!(result.applied, 0);
        assert_eq!(result.matches, 0);
    }
}
