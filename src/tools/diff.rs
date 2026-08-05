// diff 模块 —— 文件变更的 diff 计算。
//
// 使用 Myers/LCS diff 生成统一 diff，
// 包含行统计。
//
// 底层使用 `similar` crate 的 CaptureDiff。

/// 一次文件变更的完整描述。
#[derive(Debug, Clone)]
pub struct FileChange {
    /// 文件路径。
    pub path: String,
    /// 修改前内容。
    pub old_content: String,
    /// 修改后内容。
    pub new_content: String,
    /// 新增行数。
    pub added: usize,
    /// 删除行数。
    pub removed: usize,
    /// 是否为二进制文件。
    pub binary: bool,
    /// 统一 diff 格式的文本。
    pub unified_diff: String,
}

/// 变更类型标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 文件创建。
    Create,
    /// 文件修改。
    Modify,
    /// 文件删除。
    Delete,
}

/// 计算两个文本之间的统一 diff。
///
pub fn build_diff(path: &str, old_text: &str, new_text: &str, kind: Kind) -> FileChange {
    use similar::{ChangeTag, TextDiff};

    // 二进制检测（包含 NUL 字节）
    let binary = old_text.contains('\0') || new_text.contains('\0');

    let (added, removed, unified_diff) = if binary {
        // 二进制文件：仅统计字节数变化
        let added = new_text.len().saturating_sub(old_text.len());
        let removed = old_text.len().saturating_sub(new_text.len());
        let unified_diff = format!(
            "Binary files differ\n  old: {} bytes\n  new: {} bytes",
            old_text.len(),
            new_text.len()
        );
        (added, removed, unified_diff)
    } else {
        let diff = TextDiff::from_lines(old_text, new_text);
        let mut added = 0usize;
        let mut removed = 0usize;
        let mut diff_lines = Vec::new();

        // 头部
        let path_str = if path.is_empty() { "unknown" } else { path };
        match kind {
            Kind::Create => diff_lines.push(format!(
                "--- /dev/null\n+++ b/{}\n@@ -0,0 +1,{} @@",
                path_str,
                new_text.lines().count()
            )),
            Kind::Delete => diff_lines.push(format!(
                "--- a/{}\n+++ /dev/null\n@@ -1,{} +0,0 @@",
                path_str,
                old_text.lines().count()
            )),
            Kind::Modify => {
                let old_lines = old_text.lines().count();
                let new_lines = new_text.lines().count();
                diff_lines.push(format!(
                    "--- a/{}\n+++ b/{}\n@@ -1,{} +1,{} @@\n",
                    path_str, path_str, old_lines, new_lines
                ));
            }
        }

        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Equal => ' ',
                ChangeTag::Insert => {
                    added += 1;
                    '+'
                }
                ChangeTag::Delete => {
                    removed += 1;
                    '-'
                }
            };
            diff_lines.push(format!("{}{}", sign, change.value()));
        }

        (added, removed, diff_lines.concat())
    };

    FileChange {
        path: path.to_string(),
        old_content: old_text.to_string(),
        new_content: new_text.to_string(),
        added,
        removed,
        binary,
        unified_diff,
    }
}

/// 生成简洁的行统计摘要，供工具返回消息使用。
pub fn change_summary(change: &FileChange) -> String {
    let kind_str = if change.binary {
        "binary"
    } else if change.old_content.is_empty() {
        "created"
    } else if change.new_content.is_empty() {
        "deleted"
    } else {
        "modified"
    };

    if change.binary {
        format!(
            "{} {}  (binary, +{} / -{} bytes)",
            change.path, kind_str, change.added, change.removed
        )
    } else {
        format!(
            "{} {}  ({} lines added, {} lines removed)",
            change.path, kind_str, change.added, change.removed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_diff_simple() {
        let old = "hello\nworld\n";
        let new = "hello\nthere\nworld\n";
        let change = build_diff("test.txt", old, new, Kind::Modify);
        assert_eq!(change.added, 1);
        assert_eq!(change.removed, 0);
        assert!(!change.binary);
        assert!(change.unified_diff.contains("+there"));
    }

    #[test]
    fn build_diff_delete() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nline3\n";
        let change = build_diff("test.txt", old, new, Kind::Modify);
        assert_eq!(change.added, 0);
        assert_eq!(change.removed, 1);
    }

    #[test]
    fn build_diff_create() {
        let old = "";
        let new = "new file\ncontent\n";
        let change = build_diff("new.txt", old, new, Kind::Create);
        assert_eq!(change.added, 2);
        assert_eq!(change.removed, 0);
    }

    #[test]
    fn build_diff_binary() {
        let old = "hello\0world";
        let new = "hello\0there";
        let change = build_diff("bin.dat", old, new, Kind::Modify);
        assert!(change.binary);
        assert!(change.unified_diff.contains("Binary files differ"));
    }

    #[test]
    fn change_summary_modified() {
        let old = "hello\n";
        let new = "hello\nworld\n";
        let change = build_diff("test.txt", old, new, Kind::Modify);
        let summary = change_summary(&change);
        assert!(summary.contains("test.txt"));
        assert!(summary.contains("modified"));
        assert!(summary.contains("1 lines added"));
    }

    #[test]
    fn change_summary_created() {
        let old = "";
        let new = "content\n";
        let change = build_diff("new.txt", old, new, Kind::Create);
        let summary = change_summary(&change);
        assert!(summary.contains("created"));
    }
}
