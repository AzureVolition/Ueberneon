
pub mod internal;
pub mod snapshot;
pub mod diff;
pub mod jobs;
pub mod registry;
pub mod sandbox;

use std::sync::Arc;
use std::time::Duration;

pub use internal::bash::Bash;
pub use internal::bash_output::BashOutput;
pub use internal::edit_file::EditFile;
pub use internal::kill_shell::KillShell;
pub use internal::multi_edit::MultiEdit;
pub use internal::read_file::ReadFile;
pub use internal::write_file::WriteFile;
pub use snapshot::SnapshotStore;
pub use jobs::JobManager;
pub use registry::Registry;
pub use sandbox::SandboxSpec;

/// 注册所有内置工具到给定的 Registry 中。
pub fn register_builtins(registry: &Registry) {
    registry.add(Box::new(ReadFile::new()));

    // 共享状态：JobManager（后台任务）、SnapshotStore（文件快照）
    let job_manager = Arc::new(JobManager::new());
    let snapshot = Arc::new(SnapshotStore::new());

    let work_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());

    // 沙箱：默认基于工作目录创建沙箱配置
    let sandbox = SandboxSpec::defaults(&work_dir);

    registry.add(Box::new(Bash::new(
        work_dir.clone(),
        Duration::from_secs(120),
        job_manager.clone(),
        sandbox,
    )));

    registry.add(Box::new(BashOutput::new(job_manager.clone())));
    registry.add(Box::new(KillShell::new(job_manager)));

    // 文件编辑工具（共享 work_dir + snapshot）
    registry.add(Box::new(EditFile::new(work_dir.clone(), snapshot.clone())));
    registry.add(Box::new(MultiEdit::new(work_dir.clone(), snapshot.clone())));
    registry.add(Box::new(WriteFile::new(work_dir, snapshot)));
}
