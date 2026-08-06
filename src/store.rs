// ── 数据持久化层 ──
//
// 所有项目数据存储在 ~/.ueberneon/ 目录下：
//   projects.json        ← 完整项目列表（含对话）
//
// 应用启动时自动加载，数据变更时自动写盘。

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::model::Project;

/// 获取数据根目录 `~/.ueberneon/`
fn data_dir() -> PathBuf {
    let home = home_dir();
    PathBuf::from(home).join(".ueberneon")
}

/// 确保数据目录存在
fn ensure_data_dir() -> Result<PathBuf> {
    let dir = data_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create data dir: {}", dir.display()))?;
    }
    Ok(dir)
}

/// 项目清单文件路径
fn projects_path() -> PathBuf {
    data_dir().join("projects.json")
}

/// 从磁盘加载所有项目（含对话）
pub fn load_projects() -> Vec<Project> {
    let path = projects_path();
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
            tracing::warn!("failed to parse projects.json: {e}");
            Vec::new()
        }),
        Err(e) => {
            tracing::warn!("failed to read projects.json: {e}");
            Vec::new()
        }
    }
}

/// 保存所有项目（含对话）到磁盘
pub fn save_projects(projects: &[Project]) -> Result<()> {
    ensure_data_dir()?;
    let path = projects_path();
    let json = serde_json::to_string_pretty(projects).context("failed to serialize projects")?;
    std::fs::write(&path, &json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// 保存所有项目（便捷版，忽略错误）
pub fn save_projects_quiet(projects: &[Project]) {
    if let Err(e) = save_projects(projects) {
        tracing::warn!("failed to save projects: {e}");
    }
}

/// 获取用户主目录
fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Conversation;

    #[test]
    fn test_roundtrip() {
        let projects = vec![Project {
            id: "proj-1".into(),
            name: "test".into(),
            path: "/tmp/test".into(),
            created_at: chrono::Local::now(),
            conversations: vec![Conversation {
                id: "conv-1".into(),
                title: "hello".into(),
                messages: vec![],
                updated_at: chrono::Local::now(),
                message_count: 0,
            }],
            indicator_color: String::new(),
            last_activity_at: None,
        }];

        let json = serde_json::to_string_pretty(&projects).unwrap();
        let loaded: Vec<Project> = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "test");
        assert_eq!(loaded[0].conversations.len(), 1);
        assert_eq!(loaded[0].conversations[0].title, "hello");
    }
}
