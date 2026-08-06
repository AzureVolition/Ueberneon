// content_tracker.rs —— 文件内容追踪与编辑循环守卫。
//
// 两个功能：
// 1. 陈旧锚点检查（Stale Anchor Check）
//    追踪代理最后一次读取文件时的内容哈希。编辑前比对当前文件内容，
//    若不一致则要求重新读取，防止编辑过期内容。
//
// 2. 循环守卫（Loop Guard）
//    记录已应用的编辑指纹 (path, old_string, new_string)，检测并阻止
//    代理重复发送完全相同的编辑请求。

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::RwLock;

/// 循环守卫的编辑记录上限（防止内存泄漏）。
const MAX_EDIT_HISTORY: usize = 1000;

/// 文件内容追踪器，线程安全（RwLock 保护）。
pub struct FileObserveTracker {
    /// path → content hash 映射。
    /// 由 read_file 在成功读取后写入，写工具在写入成功后更新。
    observed: RwLock<HashMap<String, u64>>,

    /// 已应用的编辑签名集合。
    /// 每个签名是 (path, old_string, new_string) 的哈希。
    /// 超过 MAX_EDIT_HISTORY 时淘汰最旧的条目。
    edit_history: RwLock<VecDeque<u64>>,
    edit_set: RwLock<HashSet<u64>>,
}

impl FileObserveTracker {
    /// 创建一个新的追踪器。
    pub fn new() -> Self {
        Self {
            observed: RwLock::new(HashMap::new()),
            edit_history: RwLock::new(VecDeque::with_capacity(MAX_EDIT_HISTORY + 1)),
            edit_set: RwLock::new(HashSet::new()),
        }
    }

    /// 计算文件内容的 64 位哈希。
    fn content_hash(content: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }

    /// 计算编辑签名的哈希。
    fn edit_signature(path: &str, old_string: &str, new_string: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        old_string.hash(&mut hasher);
        new_string.hash(&mut hasher);
        hasher.finish()
    }

    // ── 陈旧锚点检查 ──

    /// 记录一次文件观察（由 read_file 在成功读取后调用）。
    pub fn observe(&self, path: &str, content: &str) {
        let mut observed = self.observed.write().expect("observed lock poisoned");
        observed.insert(path.to_string(), Self::content_hash(content));
    }

    /// 检查文件内容是否与上次观察一致。
    ///
    /// # 返回
    /// - `Ok(())` — 内容一致或文件从未被观察过（首次编辑允许通过）
    /// - `Err(msg)` — 内容已变更，返回建议重新读取的错误信息
    pub fn check_anchor(&self, path: &str, content: &str) -> Result<(), String> {
        let observed = self.observed.read().expect("observed lock poisoned");
        let current_hash = Self::content_hash(content);

        match observed.get(path) {
            // 从未观察过 → 允许通过（首次操作）
            None => Ok(()),
            // 哈希一致 → 允许
            Some(h) if *h == current_hash => Ok(()),
            // 哈希不一致 → 内容已变更
            Some(_) => Err(format!(
                "file content has changed since it was last read — the stored anchor no longer matches.\n\
                 Re-read the file with `read_file` to confirm its current content before retrying the edit."
            )),
        }
    }

    /// 更新观察哈希（由写工具在成功写入后调用）。
    ///
    /// 这使后续的编辑操作能够基于新内容继续，而不需要重新读取。
    pub fn record_write(&self, path: &str, content: &str) {
        let mut observed = self.observed.write().expect("observed lock poisoned");
        observed.insert(path.to_string(), Self::content_hash(content));
    }

    /// 清除文件的观察记录（用于测试或重置）。
    pub fn forget(&self, path: &str) {
        let mut observed = self.observed.write().expect("observed lock poisoned");
        observed.remove(path);
    }

    // ── 循环守卫 ──

    /// 检查一次编辑操作是否重复（已被应用过）。
    ///
    /// # 返回
    /// - `Ok(())` — 新编辑，允许执行
    /// - `Err(msg)` — 重复编辑，阻止并提示
    pub fn check_loop(&self, path: &str, old_string: &str, new_string: &str) -> Result<(), String> {
        let sig = Self::edit_signature(path, old_string, new_string);
        let set = self.edit_set.read().expect("edit_set lock poisoned");
        if set.contains(&sig) {
            return Err(format!(
                "this exact edit was already applied to `{}` — it is a no-op.\n\
                 The old_string you provided no longer exists in the file; re-read the file to see the current content.",
                path
            ));
        }
        Ok(())
    }

    /// 记录一次已应用的编辑。
    ///
    /// 当编辑成功写入文件后调用，将编辑签名加入历史记录。
    /// 超过 MAX_EDIT_HISTORY 条时自动淘汰最旧记录。
    pub fn record_edit(&self, path: &str, old_string: &str, new_string: &str) {
        let sig = Self::edit_signature(path, old_string, new_string);

        let mut history = self
            .edit_history
            .write()
            .expect("edit_history lock poisoned");
        let mut set = self.edit_set.write().expect("edit_set lock poisoned");

        // 如果已达上限，淘汰最旧的
        if history.len() >= MAX_EDIT_HISTORY {
            if let Some(oldest) = history.pop_front() {
                set.remove(&oldest);
            }
        }

        // 如果是新签名才插入
        if set.insert(sig) {
            history.push_back(sig);
        }
    }

    // ── 辅助方法 ──

    /// 返回当前观察的文件数量。
    pub fn observed_count(&self) -> usize {
        self.observed.read().expect("observed lock poisoned").len()
    }

    /// 返回已记录的编辑历史数量。
    pub fn edit_history_count(&self) -> usize {
        self.edit_history
            .read()
            .expect("edit_history lock poisoned")
            .len()
    }

    /// 清空所有状态（测试用）。
    pub fn clear(&self) {
        self.observed
            .write()
            .expect("observed lock poisoned")
            .clear();
        self.edit_history
            .write()
            .expect("edit_history lock poisoned")
            .clear();
        self.edit_set
            .write()
            .expect("edit_set lock poisoned")
            .clear();
    }
}

impl Default for FileObserveTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 陈旧锚点检查 ──

    #[test]
    fn observe_then_check_anchor_ok() {
        let t = FileObserveTracker::new();
        t.observe("main.rs", "fn main() {}");
        assert!(t.check_anchor("main.rs", "fn main() {}").is_ok());
    }

    #[test]
    fn never_observed_returns_ok() {
        let t = FileObserveTracker::new();
        // 未被观察过的文件 → 允许
        assert!(t.check_anchor("new.rs", "anything").is_ok());
    }

    #[test]
    fn content_changed_returns_err() {
        let t = FileObserveTracker::new();
        t.observe("main.rs", "fn main() {}");
        let result = t.check_anchor("main.rs", "fn main() { println!(\"hi\"); }");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("has changed"));
    }

    #[test]
    fn record_write_updates_hash() {
        let t = FileObserveTracker::new();
        t.observe("main.rs", "old content");
        t.record_write("main.rs", "new content");
        // 写入后更新了哈希，应该能通过检查
        assert!(t.check_anchor("main.rs", "new content").is_ok());
        // 旧内容反而会失败
        assert!(t.check_anchor("main.rs", "old content").is_err());
    }

    #[test]
    fn forget_clears_observation() {
        let t = FileObserveTracker::new();
        t.observe("main.rs", "content");
        assert!(t.check_anchor("main.rs", "different").is_err());
        t.forget("main.rs");
        assert!(t.check_anchor("main.rs", "different").is_ok());
    }

    // ── 循环守卫 ──

    #[test]
    fn first_edit_is_ok() {
        let t = FileObserveTracker::new();
        assert!(t.check_loop("main.rs", "old", "new").is_ok());
    }

    #[test]
    fn duplicate_edit_is_rejected() {
        let t = FileObserveTracker::new();
        t.record_edit("main.rs", "old", "new");
        let result = t.check_loop("main.rs", "old", "new");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already applied"));
    }

    #[test]
    fn same_edit_different_path_allowed() {
        let t = FileObserveTracker::new();
        t.record_edit("a.rs", "old", "new");
        // 不同路径 → 不同签名 → 允许
        assert!(t.check_loop("b.rs", "old", "new").is_ok());
    }

    #[test]
    fn same_edit_different_new_string_allowed() {
        let t = FileObserveTracker::new();
        t.record_edit("main.rs", "old", "new1");
        // 不同 new_string → 不同签名 → 允许
        assert!(t.check_loop("main.rs", "old", "new2").is_ok());
    }

    #[test]
    fn edit_history_bounded() {
        let t = FileObserveTracker::new();
        // 插入超过上限的编辑记录
        for i in 0..MAX_EDIT_HISTORY + 50 {
            t.record_edit(&format!("f{}.rs", i), "old", &format!("new{}", i));
        }
        assert!(t.edit_history_count() <= MAX_EDIT_HISTORY);
        // 最旧的记录应该被淘汰了
        assert!(t.check_loop("f0.rs", "old", "new0").is_ok());
        // 最新的记录应该还在
        let last = format!("new{}", MAX_EDIT_HISTORY + 49);
        assert!(
            t.check_loop(&format!("f{}.rs", MAX_EDIT_HISTORY + 49), "old", &last)
                .is_err()
        );
    }

    #[test]
    fn clear_resets_all() {
        let t = FileObserveTracker::new();
        t.observe("main.rs", "content");
        t.record_edit("main.rs", "old", "new");
        t.clear();
        assert_eq!(t.observed_count(), 0);
        assert_eq!(t.edit_history_count(), 0);
        assert!(t.check_loop("main.rs", "old", "new").is_ok());
        assert!(t.check_anchor("main.rs", "different").is_ok());
    }

    // ── 真实场景 ──

    #[test]
    fn read_edit_read_edit_flow() {
        let t = FileObserveTracker::new();

        // 1. 读取文件
        t.observe("lib.rs", "fn old_func() {}");

        // 2. 第一次编辑 → 通过
        assert!(t.check_anchor("lib.rs", "fn old_func() {}").is_ok());
        assert!(
            t.check_loop("lib.rs", "fn old_func() {}", "fn new_func() {}")
                .is_ok()
        );
        t.record_edit("lib.rs", "fn old_func() {}", "fn new_func() {}");
        t.record_write("lib.rs", "fn new_func() {}");

        // 3. 再次读取（代理确认修改结果）
        t.observe("lib.rs", "fn new_func() {}");

        // 4. 第二次编辑 → 通过
        assert!(t.check_anchor("lib.rs", "fn new_func() {}").is_ok());
        assert!(
            t.check_loop("lib.rs", "fn new_func() {}", "fn final_func() {}")
                .is_ok()
        );
        t.record_edit("lib.rs", "fn new_func() {}", "fn final_func() {}");
        t.record_write("lib.rs", "fn final_func() {}");

        // 5. 尝试重复第一次编辑 → 被循环守卫阻止
        let result = t.check_loop("lib.rs", "fn old_func() {}", "fn new_func() {}");
        assert!(result.is_err());
    }
}
