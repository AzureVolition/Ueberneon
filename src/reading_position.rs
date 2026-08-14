// ── 阅读位置持久化 ──
//
// 记录每本书上次阅读到的页码，保存在书目录的 last_page.json 中，
// 不引入数据库迁移。

use std::path::Path;

fn last_page_path(book_dir: &Path) -> std::path::PathBuf {
    book_dir.join("last_page.json")
}

/// 保存阅读位置（原子写：临时文件 + rename）。
pub fn save(book_dir: &Path, page: u32) {
    if page == 0 {
        return;
    }
    let path = last_page_path(book_dir);
    let json = match serde_json::to_string_pretty(&serde_json::json!({ "page": page })) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "reader", error = %e, "serialize last page failed");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, json) {
        tracing::warn!(target: "reader", error = %e, "save last page failed");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!(target: "reader", error = %e, "publish last page failed");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 读取上次阅读位置；缺失/损坏返回 None。
pub fn load(book_dir: &Path) -> Option<u32> {
    let path = last_page_path(book_dir);
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value.get("page").and_then(|v| v.as_u64()).map(|p| p as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "ueberneon-reading-pos-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        save(&dir, 42);
        assert_eq!(load(&dir), Some(42));
        save(&dir, 7);
        assert_eq!(load(&dir), Some(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = std::env::temp_dir().join(format!(
            "ueberneon-reading-pos-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
