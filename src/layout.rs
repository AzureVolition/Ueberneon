// ── 应用目录布局 ──
//
// 所有用户内容统一放在 ~/.ueberneon/ 下：
//   ~/.ueberneon/data.db               SQLite 数据库
//   ~/.ueberneon/settings.json         应用设置
//   ~/.ueberneon/books/<书目录>/        全局书库（项目引用，不复制）
//   ~/.ueberneon/projects/<项目>/      项目目录（Agent 工作区）
//   ~/.ueberneon/projects/<项目>/note/ 项目笔记

use std::path::{Path, PathBuf};

/// 用户主目录（可被测试通过 HOME 覆盖）
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// ueberneon 数据根目录 ~/.ueberneon
pub fn ueberneon_home() -> PathBuf {
    home_dir().join(".ueberneon")
}

/// 项目根目录 ~/.ueberneon/projects
pub fn projects_root() -> PathBuf {
    ueberneon_home().join("projects")
}

/// 全局书库根目录 ~/.ueberneon/books
pub fn books_root() -> PathBuf {
    ueberneon_home().join("books")
}

/// 单本书的目录:~/.ueberneon/books/<book_id>/
/// 目录名使用 books 表 id,显示名只存在 books.name,避免复杂字符进路径。
pub fn book_dir(book_id: &str) -> PathBuf {
    books_root().join(book_id)
}

/// 书目录内的原始 PDF 路径
pub fn book_pdf_path(book_dir: &Path) -> PathBuf {
    book_dir.join("original.pdf")
}

/// 书目录内的知识库文本目录:<book_dir>/pages/
pub fn book_pages_dir(book_dir: &Path) -> PathBuf {
    book_dir.join("pages")
}

/// 单页 MD 文件路径:<pages_dir>/0001.md(页码从 1 开始,四位补零)
pub fn book_page_md_path(pages_dir: &Path, page_1based: u32) -> PathBuf {
    pages_dir.join(format!("{page_1based:04}.md"))
}

/// 解析完成标记:<book_dir>/parsed.json
pub fn book_parse_marker_path(book_dir: &Path) -> PathBuf {
    book_dir.join("parsed.json")
}

/// 书目录内的 OCR 缓存目录:<book_dir>/ocr/
pub fn book_ocr_dir(book_dir: &Path) -> PathBuf {
    book_dir.join("ocr")
}

/// 单页 OCR 词行文件路径:<ocr_dir>/<NNNN>.json(页码从 1 开始)
pub fn book_ocr_page_path(ocr_dir: &Path, page_1based: u32) -> PathBuf {
    ocr_dir.join(format!("{page_1based:04}.json"))
}

/// OCR 进度文件:<book_dir>/ocr/progress.json
pub fn book_ocr_progress_path(book_dir: &Path) -> PathBuf {
    book_ocr_dir(book_dir).join("progress.json")
}

/// 默认项目目录 ~/.ueberneon/projects/ueberneon-default
pub fn default_project_dir() -> PathBuf {
    projects_root().join("ueberneon-default")
}

/// 项目内的笔记目录
pub fn project_note_dir(project_dir: &Path) -> PathBuf {
    project_dir.join("note")
}

/// 清洗目录名：替换非法/危险字符为 `-`，空名回退为 `project`
pub fn sanitize_dir_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        let illegal =
            ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
        if illegal {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(ch);
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "project".into()
    } else {
        out
    }
}

/// 在 root 下挑选不冲突的项目目录：名字冲突或路径已存在时追加 `-2`、`-3`…
pub fn unique_project_dir(root: &Path, name: &str, existing_paths: &[PathBuf]) -> PathBuf {
    let base = sanitize_dir_name(name);
    let mut candidate = root.join(&base);
    let mut i = 2;
    while candidate.exists()
        || existing_paths
            .iter()
            .any(|p| p.as_path() == candidate.as_path())
    {
        candidate = root.join(format!("{base}-{i}"));
        i += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_illegal_chars() {
        assert_eq!(sanitize_dir_name("  my math  "), "my math");
        assert_eq!(
            sanitize_dir_name("a/b\\c:d*e?f\"g<h>i|j"),
            "a-b-c-d-e-f-g-h-i-j"
        );
        assert_eq!(sanitize_dir_name("   "), "project");
        assert_eq!(sanitize_dir_name("线性代数"), "线性代数");
        assert_eq!(sanitize_dir_name("a//b"), "a-b");
    }

    #[test]
    fn unique_dir_dedupes() {
        let root =
            std::env::temp_dir().join(format!("ueberneon-layout-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("math")).unwrap();

        let existing = vec![root.join("math")];
        let first = unique_project_dir(&root, "math", &existing);
        assert_eq!(first, root.join("math-2"));

        std::fs::create_dir_all(&first).unwrap();
        let second = unique_project_dir(&root, "math", &existing);
        assert_eq!(second, root.join("math-3"));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
