pub mod content_tracker;
pub mod diff;
pub mod internal;
pub mod jobs;
pub mod registry;
pub mod sandbox;
pub mod snapshot;

use crate::permission::Check;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

pub use internal::bash::Bash;
pub use internal::bash_output::BashOutput;
pub use internal::code_index::CodeIndex;
pub use internal::complete_step::CompleteStep;
pub use internal::create_plan::CreatePlan;
pub use internal::edit_file::EditFile;
pub use internal::glob::Glob;
pub use internal::grep::Grep;
pub use internal::kill_shell::KillShell;
pub use internal::load_skill::LoadSkill;
pub use internal::ls::Ls;
pub use internal::multi_edit::MultiEdit;
pub use internal::read_file::ReadFile;
pub use internal::read_only_bash::ReadOnlyBash;
pub use internal::task::Task;
pub use internal::web_fetch::WebFetch;
pub use internal::write_file::WriteFile;
pub use jobs::JobManager;
pub use registry::Registry;
pub use sandbox::SandboxSpec;
pub use snapshot::SnapshotStore;

/// 内部工具的编译时元信息。
/// 每个 `#[derive(ToolMetaImpl)]` 结构体会通过 `inventory::submit!` 自动注册。
pub struct InternalToolMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub schema: &'static LazyLock<String>,
}

inventory::collect!(InternalToolMeta);

/// 注册所有内置工具到给定的 Registry 中。
/// `base_dir` 是工具的工作目录（即项目路径）。
pub fn register_builtins(registry: &Registry, base_dir: &std::path::Path) {
    // 文件内容追踪器（陈旧锚点 + 循环守卫）
    let tracker = Arc::new(content_tracker::FileObserveTracker::new());

    // 共享状态：JobManager（后台任务）、SnapshotStore（文件快照）
    let job_manager = Arc::new(JobManager::new());
    let snapshot = Arc::new(SnapshotStore::new());

    let work_dir = base_dir.to_path_buf();

    // 把磁盘上的技能目录同步进 DB 注册表（面板数据源）
    crate::skills::sync_registry(&work_dir);

    registry.add(Box::new(ReadFile::new(work_dir.clone(), tracker.clone())));

    // 沙箱：默认基于工作目录创建沙箱配置
    let sandbox = SandboxSpec::defaults(&work_dir);

    registry.add(Box::new(BashOutput::new(job_manager.clone())));
    registry.add(Box::new(KillShell::new(job_manager.clone())));

    let file_checks = || -> Vec<Box<dyn Check>> {
        vec![Box::new(crate::permission::checks::DenySystemPaths) as Box<dyn Check>]
    };

    let bash_checks = || -> Vec<Box<dyn Check>> {
        vec![
            Box::new(crate::permission::checks::ForcePushGuard) as Box<dyn Check>,
            Box::new(crate::permission::checks::DangerousPatternDetector) as Box<dyn Check>,
        ]
    };

    registry.add(Box::new(Bash::new(
        work_dir.clone(),
        Duration::from_secs(120),
        job_manager.clone(),
        Some(sandbox.clone()),
        bash_checks(),
    )));

    // 文件编辑工具（注入可复用的权限检查：DenySystemPaths）
    registry.add(Box::new(EditFile::new(
        work_dir.clone(),
        snapshot.clone(),
        file_checks(),
        tracker.clone(),
    )));
    registry.add(Box::new(MultiEdit::new(
        work_dir.clone(),
        snapshot.clone(),
        file_checks(),
        tracker.clone(),
    )));
    registry.add(Box::new(WriteFile::new(
        work_dir.clone(),
        snapshot,
        file_checks(),
        tracker,
    )));

    // 搜索工具
    registry.add(Box::new(Grep::new(work_dir.clone())));

    // 代码浏览与索引工具
    registry.add(Box::new(Ls::new(work_dir.clone())));
    registry.add(Box::new(Glob::new(work_dir.clone())));
    registry.add(Box::new(CodeIndex::new(work_dir.clone())));

    // 网络工具
    registry.add(Box::new(WebFetch::new()));

    // 计划工具
    registry.add(Box::new(CreatePlan));
    registry.add(Box::new(CompleteStep));

    // 只读 bash（用于 subagent / explore）
    registry.add(Box::new(ReadOnlyBash::new(
        work_dir.to_path_buf(),
        Duration::from_secs(30),
        Some(sandbox),
    )));

    // 子 Agent 委派工具
    registry.add(Box::new(Task::new()));

    // 技能加载工具
    registry.add(Box::new(LoadSkill::new(work_dir)));
}
