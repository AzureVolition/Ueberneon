// jobs/mod.rs —— 后台任务管理器。
//
// 管理跨轮次存活的子进程，
// 支持 spawn（返回 job ID）、增量读取输出、kill、reap。
// 每个后台任务由独立的 tokio task 驱动，持续收集 stdout+stderr。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// 后台任务句柄，持有进程和输出缓冲区。
pub struct JobHandle {
    /// 唯一标识（格式：`bg-<n>`）。
    pub id: String,
    /// 子进程（Mutex 保护，kill 时 take 出来）。
    child: Mutex<Option<Child>>,
    /// 累积的 stdout+stderr 字节。
    output_buf: Mutex<Vec<u8>>,
    /// 上次读取到的偏移（bash_output 用）。
    read_offset: AtomicUsize,
    /// 任务是否已结束。
    pub finished: AtomicBool,
    /// 进程退出码（-1 = 未退出）。
    pub exit_code: AtomicUsize,
    /// 启动时间。
    pub started_at: Instant,
}

impl JobHandle {
    /// 读取自上次调用以来的新输出。
    pub fn read_new_output(&self) -> String {
        let buf = self.output_buf.lock().expect("output_buf lock poisoned");
        let prev = self.read_offset.swap(buf.len(), Ordering::SeqCst);
        if prev >= buf.len() {
            return String::new();
        }
        // 尝试 UTF-8 解码；失败时用 lossy
        String::from_utf8_lossy(&buf[prev..]).into_owned()
    }

    /// 终止后台任务（先 SIGTERM，后 SIGKILL）。
    pub async fn kill(&self) {
        // take 出子进程，释放 MutexGuard 后再做 async 操作
        let child = { self.child.lock().expect("child lock poisoned").take() };

        if let Some(mut child) = child {
            let pid = child.id().unwrap_or(0);
            // SIGTERM
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            // 等待 200ms
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            // SIGKILL
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            // 尝试 wait 清理僵尸进程
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.finished.store(true, Ordering::SeqCst);
    }
}

/// 后台任务管理器。
///
/// 全局单例，通过 `Arc<JobManager>` 在 bash / bash_output / kill_shell 间共享。
pub struct JobManager {
    jobs: RwLock<HashMap<String, Arc<JobHandle>>>,
    next_id: AtomicU64,
}

impl JobManager {
    pub fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// 启动后台命令，返回 job ID。
    ///
    /// 命令在 shell 中执行（program + args 已由调用方包装好）。
    /// spawn 启动一个 tokio task 持续读取子进程输出。
    pub async fn spawn(
        self: &Arc<Self>,
        program: &str,
        args: &[String],
        work_dir: &std::path::Path,
    ) -> String {
        let id_num = self.next_id.fetch_add(1, Ordering::SeqCst);
        let job_id = format!("bg-{id_num}");

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // 启用进程组
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                let _ = libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // spawn 失败：创建一个已完成的 dummy handle，输出错误信息
                let handle = Arc::new(JobHandle {
                    id: job_id.clone(),
                    child: Mutex::new(None),
                    output_buf: Mutex::new(
                        format!("bash: failed to spawn background command: {e}").into_bytes(),
                    ),
                    read_offset: AtomicUsize::new(0),
                    finished: AtomicBool::new(true),
                    exit_code: AtomicUsize::new(usize::MAX),
                    started_at: Instant::now(),
                });
                self.jobs
                    .write()
                    .expect("jobs lock poisoned")
                    .insert(job_id.clone(), handle);
                return job_id;
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // 创建 handle，放入 HashMap
        let handle = Arc::new(JobHandle {
            id: job_id.clone(),
            child: Mutex::new(Some(child)),
            output_buf: Mutex::new(Vec::new()),
            read_offset: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            exit_code: AtomicUsize::new(usize::MAX),
            started_at: Instant::now(),
        });

        self.jobs
            .write()
            .unwrap()
            .insert(job_id.clone(), handle.clone());

        // 启动后台 reader task
        let h = handle.clone();
        tokio::spawn(async move {
            Self::reader_task(h, stdout, stderr).await;
        });

        job_id
    }

    /// 获取 job handle。
    pub fn get(&self, job_id: &str) -> Option<Arc<JobHandle>> {
        self.jobs
            .read()
            .expect("jobs lock poisoned")
            .get(job_id)
            .cloned()
    }

    /// 列出所有活跃的 job ID。
    pub fn list_ids(&self) -> Vec<String> {
        self.jobs
            .read()
            .expect("jobs lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// 清理已完成的 job（调用 kill 后或进程自然退出后）。
    pub fn reap(&self) -> usize {
        let mut jobs = self.jobs.write().expect("jobs lock poisoned");
        let before = jobs.len();
        jobs.retain(|_, h| !h.finished.load(Ordering::SeqCst));
        before - jobs.len()
    }

    /// 后台 reader：持续读取 stdout + stderr，直到进程退出。
    async fn reader_task(
        handle: Arc<JobHandle>,
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
    ) {
        let mut out_reader = stdout;
        let mut err_reader = stderr;

        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];

        loop {
            let mut did_read = false;

            // 读 stdout
            if let Some(ref mut reader) = out_reader {
                match reader.read(&mut out_buf).await {
                    Ok(0) => {
                        // EOF
                        out_reader = None;
                        did_read = true;
                    }
                    Ok(n) => {
                        if n > 0 {
                            let mut buf =
                                handle.output_buf.lock().expect("output_buf lock poisoned");
                            buf.extend_from_slice(&out_buf[..n]);
                            did_read = true;
                        }
                    }
                    Err(_) => {
                        out_reader = None;
                    }
                }
            }

            // 读 stderr
            if let Some(ref mut reader) = err_reader {
                match reader.read(&mut err_buf).await {
                    Ok(0) => {
                        err_reader = None;
                        did_read = true;
                    }
                    Ok(n) => {
                        if n > 0 {
                            let mut buf =
                                handle.output_buf.lock().expect("output_buf lock poisoned");
                            buf.extend_from_slice(&err_buf[..n]);
                            did_read = true;
                        }
                    }
                    Err(_) => {
                        err_reader = None;
                    }
                }
            }

            // 两边都 EOF 了 → 进程退出
            if out_reader.is_none() && err_reader.is_none() {
                break;
            }

            // 避免忙等
            if !did_read {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }

        // 等待子进程真正退出，记录 exit_code
        // take 出子进程，释放 MutexGuard 后再 await
        let child = { handle.child.lock().expect("child lock poisoned").take() };
        if let Some(mut child) = child {
            match child.wait().await {
                Ok(status) => {
                    handle
                        .exit_code
                        .store(status.code().unwrap_or(-1) as usize, Ordering::SeqCst);
                }
                Err(_) => {
                    handle.exit_code.store(usize::MAX, Ordering::SeqCst);
                }
            }
        }
        handle.finished.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn spawn_simple_command() {
        let mgr = Arc::new(JobManager::new());
        let job_id = mgr
            .spawn(
                "echo",
                &["hello_bg".into()],
                &std::env::current_dir().unwrap(),
            )
            .await;

        assert!(job_id.starts_with("bg-"));

        // 等待进程完成
        tokio::time::sleep(Duration::from_millis(500)).await;

        let handle = mgr.get(&job_id).unwrap();
        let output = handle.read_new_output();
        assert!(
            output.contains("hello_bg"),
            "expected 'hello_bg' in output, got: '{output}'"
        );
    }

    #[tokio::test]
    async fn read_new_output_incremental() {
        let mgr = Arc::new(JobManager::new());
        let job_id = mgr
            .spawn(
                "sh",
                &["-c".into(), "echo first; sleep 1; echo second".into()],
                &std::env::current_dir().unwrap(),
            )
            .await;

        // 等一小会儿读第一部分
        tokio::time::sleep(Duration::from_millis(300)).await;
        let h = mgr.get(&job_id).unwrap();
        let out1 = h.read_new_output();
        assert!(out1.contains("first"), "out1: {out1}");

        // 再等一会读第二部分
        tokio::time::sleep(Duration::from_secs(1)).await;
        let out2 = h.read_new_output();
        assert!(out2.contains("second"), "out2: {out2}");
    }

    #[tokio::test]
    async fn kill_running_job() {
        let mgr = Arc::new(JobManager::new());
        let job_id = mgr
            .spawn("sleep", &["10".into()], &std::env::current_dir().unwrap())
            .await;

        let handle = mgr.get(&job_id).unwrap();
        assert!(!handle.finished.load(Ordering::SeqCst));

        handle.kill().await;
        // 给一点时间让 kill 生效
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(handle.finished.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn list_and_reap() {
        let mgr = Arc::new(JobManager::new());
        let j1 = mgr
            .spawn("echo", &["done".into()], &std::env::current_dir().unwrap())
            .await;

        let ids = mgr.list_ids();
        assert!(ids.contains(&j1));

        // 等待进程结束
        tokio::time::sleep(Duration::from_millis(500)).await;

        let reaped = mgr.reap();
        assert_eq!(reaped, 1);
    }
}
