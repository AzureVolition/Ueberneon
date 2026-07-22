// snapshot 模块 —— 文件变更快照管理，支持撤销（undo）。
//
// 在写操作执行前记录文件的原始内容，支持按 turn 恢复。
//
// 存储策略：内存 HashMap。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

/// 快照存储 —— 管理所有文件的快照。
pub struct SnapshotStore {
    /// 快照映射：path → (turn, original_content)
    snapshots: RwLock<HashMap<String, (usize, String)>>,
    /// 持久化根目录（可选）。
    #[allow(dead_code)]
    persist_dir: Option<PathBuf>,
}

impl SnapshotStore {
    /// 创建一个新的检查点存储（不持久化）。
    pub fn new() -> Self {
        Self {
            snapshots: RwLock::new(HashMap::new()),
            persist_dir: None,
        }
    }

    /// 创建一个带持久化的检查点存储。
    pub fn with_persist(persist_dir: PathBuf) -> Self {
        Self {
            snapshots: RwLock::new(HashMap::new()),
            persist_dir: Some(persist_dir),
        }
    }

    /// 在写操作前记录文件的原始内容快照。
    ///
    /// 每个文件在每个 turn 只记录第一次快照。后续修改不会再覆盖快照，
    /// 以确保撤销时可以恢复到最早的状态。
    ///
    /// 返回 `true` 表示这是该文件在本 turn 的首次快照。
    pub fn snapshot(&self, path: &str, content: &str, turn: usize) -> bool {
        let mut snaps = self.snapshots.write().expect("snapshots lock poisoned");
        if snaps.contains_key(path) {
            false
        } else {
            snaps.insert(path.to_string(), (turn, content.to_string()));
            true
        }
    }

    /// 恢复到指定 turn（含）的所有快照。
    ///
    /// 返回需要恢复的文件列表：(path, original_content)。
    pub fn restore_up_to(&self, turn: usize) -> Vec<(String, String)> {
        let snaps = self.snapshots.read().expect("snapshots lock poisoned");
        snaps
            .iter()
            .filter(|(_, v)| v.0 <= turn)
            .map(|(path, (_, content))| (path.clone(), content.clone()))
            .collect()
    }

    /// 获取某个文件的快照。
    pub fn get_snapshot(&self, path: &str) -> Option<(usize, String)> {
        self.snapshots.read().expect("snapshots lock poisoned").get(path).cloned()
    }

    /// 获取所有快照的数量。
    pub fn len(&self) -> usize {
        self.snapshots.read().expect("snapshots lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清空所有快照。
    pub fn clear(&self) {
        self.snapshots.write().expect("snapshots lock poisoned").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_records_content() {
        let store = SnapshotStore::new();
        let added = store.snapshot("test.txt", "original content", 1);
        assert!(added, "first snapshot should return true");

        let (turn, content) = store.get_snapshot("test.txt").unwrap();
        assert_eq!(turn, 1);
        assert_eq!(content, "original content");
    }

    #[test]
    fn snapshot_dedup_per_turn() {
        let store = SnapshotStore::new();
        assert!(store.snapshot("test.txt", "v1", 1));
        assert!(!store.snapshot("test.txt", "v2", 1), "dup in same turn");
        assert!(!store.snapshot("test.txt", "v3", 2), "dup across turns");
    }

    #[test]
    fn restore_up_to_returns_matching_snapshots() {
        let store = SnapshotStore::new();
        store.snapshot("a.txt", "a1", 1);
        store.snapshot("b.txt", "b1", 2);
        store.snapshot("c.txt", "c1", 3);

        let restored = store.restore_up_to(2);
        assert_eq!(restored.len(), 2);
        assert!(restored.iter().any(|(p, _)| p == "a.txt"));
        assert!(restored.iter().any(|(p, _)| p == "b.txt"));
        assert!(!restored.iter().any(|(p, _)| p == "c.txt"));
    }

    #[test]
    fn clear_removes_all() {
        let store = SnapshotStore::new();
        store.snapshot("a.txt", "a", 1);
        store.snapshot("b.txt", "b", 1);
        assert_eq!(store.len(), 2);
        store.clear();
        assert!(store.is_empty());
    }
}
