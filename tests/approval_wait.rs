//! execute() 审批等待逻辑的集成测试。
//!
//! 覆盖状态驱动收敛循环（Notify 广播）的关键路径：
//!   allow（执行）/ deny（拒绝）/ 通道关闭（自动拒绝）/ 取消（中止）/ 提前点选。
//!
//! 走 pub API 全流程（accept_message → stream_message → execute），用 fake
//! Provider 产生工具调用流、fake CheckableTool 执行；临时 HOME 隔离 DB
//! （accept_message/execute 会落库到 ~/.ueberneon/data.db）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use llm::provider::{Chunk, ChunkStream, Provider, ProviderError, Request, ToolCall};
use llm::tool::ToolMeta;
use tokio_util::sync::CancellationToken;

use ueberneon::model::{StreamSegment, ToolCallStatus};
use ueberneon::permission::Decision;
use ueberneon::state_agent::{
    Agent, AgentCore, AgentHandler, ApprovalChain, InterruptState, Running, Static, StreamResult,
    Tool, ToolContext, ToolResult, UserApprovalGate,
};
use ueberneon::tools::Registry;
use ueberneon::tools::internal::common::checkable_tool::CheckableTool;

// ── fake 组件 ───────────────────────────────────────────────────────────────

/// 可执行计数工具：execute 被调用一次 +1。
struct FakeTool {
    name: &'static str,
    runs: Arc<AtomicUsize>,
}

impl ToolMeta for FakeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "fake tool for approval tests"
    }
    fn read_only(&self) -> bool {
        false
    }
    fn schema_str_str(&self) -> &str {
        "{}"
    }
}

#[async_trait::async_trait]
impl Tool for FakeTool {
    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: &serde_json::Value,
    ) -> Result<ToolResult, String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::ok("fake tool ran"))
    }
}

impl CheckableTool for FakeTool {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}

/// fake Provider：首次调用返回给定工具调用流（全量），后续返回空流（execute 续跑）。
struct FakeProvider {
    calls: Arc<AtomicUsize>,
    tool_calls: Vec<(String, String)>, // (id, name)
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }
    async fn stream(&self, _req: &Request) -> Result<ChunkStream, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let chunks = self
                .tool_calls
                .iter()
                .map(|(id, name)| {
                    Ok(Chunk::ToolCallComplete(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: "{}".into(),
                        diff: String::new(),
                        added: 0,
                        removed: 0,
                    }))
                })
                .collect::<Vec<_>>();
            Ok(Box::pin(futures::stream::iter(chunks)))
        } else {
            Ok(Box::pin(futures::stream::iter(vec![])))
        }
    }
}

// ── 辅助 ─────────────────────────────────────────────────────────────────────

/// 临时 HOME 隔离 DB（per-pid 子目录，避免并发 cargo test 进程冲突；
/// 同进程内多个 #[tokio::test] 并行 set 同一值幂等）。
fn setup_test_home() {
    unsafe {
        std::env::set_var(
            "HOME",
            format!("/tmp/ueberneon-itest-approval-{}", std::process::id()),
        );
    }
}

fn make_agent(provider: Box<dyn Provider>, registry: Registry, tag: &str) -> Agent<Static> {
    let core = AgentCore::new(
        provider,
        registry,
        std::env::temp_dir(),
        None,
        format!("approval-test-{tag}-{}", std::process::id()),
        0.7,
        None,
        4096,
        "test".into(),
        false,
    );
    Agent {
        running: Static,
        agent: core,
    }
}

fn user_input() -> Vec<llm::Message> {
    vec![llm::Message {
        role: llm::Role::User,
        content: Some("run the tool".into()),
        ..Default::default()
    }]
}

/// accept_message + stream_message，直到 Enter Executing（有工具待执行）。
/// 返回 (executing, approval_tx, cancel_token)。
async fn drive_to_executing(
    agent: Agent<Static>,
    cancel_token: CancellationToken,
) -> (
    Agent<Running<ueberneon::state_agent::Executing>>,
    tokio::sync::mpsc::Sender<(String, bool)>,
    CancellationToken,
) {
    // 预建 conversation 行：accept_message 会把用户输入落库（messages 外键约束
    // 要求 conversation 存在；db 初始化由 get_db() 懒加载到临时 HOME）
    let conv_id = agent.agent.conversation_id.clone();
    ueberneon::db::with_db(|conn| {
        conn.execute(
            "INSERT OR IGNORE INTO conversations (id, project_id, title, updated_at, created_at)
             VALUES (?1, 'ueberneon-default', 'approval test', ?2, ?2)",
            rusqlite::params![conv_id, chrono::Local::now().to_rfc3339()],
        )
    })
    .expect("create conversation");

    let handler = AgentHandler::default();
    // 测试工具名（bash / task）在执行类名单内,gate 恒 Ask → 触发审批等待,
    // 与测试意图（审批管道注入 allow/deny/通道关闭/取消）一致,与 agent_mode 无关。
    let (running, _rx) = agent
        .accept_message(
            user_input(),
            cancel_token.clone(),
            handler,
            Box::new(ApprovalChain::new(vec![Box::new(UserApprovalGate)])),
        )
        .await
        .expect("accept_message should succeed");
    let stream_result = running
        .stream_message()
        .await
        .expect("stream should succeed");
    let StreamResult::Continue(executing) = stream_result else {
        panic!("expected Continue (tool call) but got Done");
    };
    let tx = executing
        .running
        .approval_tx
        .clone()
        .expect("approval tx should be set on Continue");
    (executing, tx, cancel_token)
}

fn records_of(
    agent: &Agent<Running<ueberneon::state_agent::Streaming>>,
) -> Vec<ueberneon::model::ToolCallRecord> {
    let segs = agent
        .running
        .streaming_handle
        .segments
        .lock()
        .expect("segments lock poisoned");
    segs.iter()
        .filter_map(|s| match s {
            StreamSegment::ToolCall(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

// ── 场景 1：allow → 工具执行 ───────────────────────────────────────────────

#[tokio::test]
async fn allow_executes_tool() {
    setup_test_home();
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new();
    registry.add(Box::new(FakeTool {
        name: "bash",
        runs: runs.clone(),
    }));
    let provider = Box::new(FakeProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        tool_calls: vec![("call_1".into(), "bash".into())],
    });
    let cancel = CancellationToken::new();

    let (executing, tx, _cancel) =
        drive_to_executing(make_agent(provider, registry, "allow"), cancel.clone()).await;

    // 并发注入 allow（等 execute 进入等待）
    let inject = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.try_send(("call_1".to_string(), true))
            .expect("send allow");
    });
    let result = executing.execute().await;
    inject.await.expect("inject task panicked");

    let agent = result.expect("execute should succeed after allow");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "tool should have run once");
    let recs = records_of(&agent);
    assert_eq!(recs.len(), 1);
    assert!(
        matches!(recs[0].status, ToolCallStatus::Success),
        "record should be Success, got {:?}",
        recs[0].status
    );
}

// ── 场景 2：deny → 拒绝落库 ───────────────────────────────────────────────

#[tokio::test]
async fn deny_rejects_tool() {
    setup_test_home();
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new();
    registry.add(Box::new(FakeTool {
        name: "bash",
        runs: runs.clone(),
    }));
    let provider = Box::new(FakeProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        tool_calls: vec![("call_1".into(), "bash".into())],
    });
    let cancel = CancellationToken::new();

    let (executing, tx, _cancel) =
        drive_to_executing(make_agent(provider, registry, "deny"), cancel.clone()).await;

    let inject = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.try_send(("call_1".to_string(), false))
            .expect("send deny");
    });
    let result = executing.execute().await;
    inject.await.expect("inject task panicked");

    let agent = result.expect("execute should succeed after deny");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "tool should not run");
    let recs = records_of(&agent);
    assert_eq!(recs.len(), 1);
    assert!(
        matches!(recs[0].status, ToolCallStatus::Denied(ref m) if m == "denied by user"),
        "record should be Denied(denied by user), got {:?}",
        recs[0].status
    );
}

// ── 场景 3：通道关闭 → 自动拒绝 ───────────────────────────────────────────

#[tokio::test]
async fn channel_close_auto_denies() {
    setup_test_home();
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new();
    registry.add(Box::new(FakeTool {
        name: "bash",
        runs: runs.clone(),
    }));
    let provider = Box::new(FakeProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        tool_calls: vec![("call_1".into(), "bash".into())],
    });
    let cancel = CancellationToken::new();

    let (executing, tx, _cancel) =
        drive_to_executing(make_agent(provider, registry, "close"), cancel.clone()).await;
    // drop 唯一 Sender → 子线程 recv None → closed → 主线程自动拒绝
    drop(tx);

    let result = executing.execute().await;
    let agent = result.expect("execute should succeed (auto-deny on channel close)");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "tool should not run");
    let recs = records_of(&agent);
    assert_eq!(recs.len(), 1);
    assert!(
        matches!(recs[0].status, ToolCallStatus::Denied(ref m) if m == "approval channel closed"),
        "record should be Denied(approval channel closed), got {:?}",
        recs[0].status
    );
}

// ── 场景 4：等待中取消 → Err(Cancelled) ───────────────────────────────────

#[tokio::test]
async fn cancel_aborts_wait() {
    setup_test_home();
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new();
    registry.add(Box::new(FakeTool {
        name: "bash",
        runs: runs.clone(),
    }));
    let provider = Box::new(FakeProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        tool_calls: vec![("call_1".into(), "bash".into())],
    });
    let cancel = CancellationToken::new();

    let (executing, _tx, cancel) =
        drive_to_executing(make_agent(provider, registry, "cancel"), cancel.clone()).await;

    let cancel_task = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        }
    });
    let result = executing.execute().await;
    cancel_task.await.expect("cancel task panicked");

    match result {
        Err(InterruptState::Cancelled) => {}
        Ok(_) => panic!("expected Err(Cancelled), got Ok"),
        Err(InterruptState::Error(e)) => panic!("expected Err(Cancelled), got Error: {e}"),
    }
    assert_eq!(runs.load(Ordering::SeqCst), 0, "tool should not run");
}

// ── 场景 5：两个工具，第二个提前点选 + 第一个正常 allow ──────────────────

#[tokio::test]
async fn pre_approved_second_tool_runs_in_order() {
    setup_test_home();
    let runs_a = Arc::new(AtomicUsize::new(0));
    let runs_b = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new();
    registry.add(Box::new(FakeTool {
        name: "bash",
        runs: runs_a.clone(),
    }));
    registry.add(Box::new(FakeTool {
        name: "task",
        runs: runs_b.clone(),
    }));
    let provider = Box::new(FakeProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        tool_calls: vec![
            ("call_a".into(), "bash".into()),
            ("call_b".into(), "task".into()),
        ],
    });
    let cancel = CancellationToken::new();

    let (executing, tx, _cancel) =
        drive_to_executing(make_agent(provider, registry, "preapprove"), cancel.clone()).await;
    // 两个工具都已进入 AwaitingApproval（阶段一先完成）。
    // execute 开始等待后并发注入：先决策第二个（unmatched 路径：子线程写
    // record=Pending + Notify 广播，主线程仍在等第一个），再决策第一个——
    // 与真实 UI 流程一致（审批卡渲染后可点击），走 Notify 唤醒路径。
    let inject = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.try_send(("call_b".to_string(), true))
            .expect("send allow b");
        tx.try_send(("call_a".to_string(), true))
            .expect("send allow a");
    });
    let result = executing.execute().await;
    inject.await.expect("inject task panicked");

    let agent = result.expect("execute should succeed");
    assert_eq!(runs_a.load(Ordering::SeqCst), 1, "tool_a should run once");
    assert_eq!(runs_b.load(Ordering::SeqCst), 1, "tool_b should run once");
    let recs = records_of(&agent);
    assert_eq!(recs.len(), 2);
    for (i, rec) in recs.iter().enumerate() {
        assert!(
            matches!(rec.status, ToolCallStatus::Success),
            "record[{i}] should be Success, got {:?}",
            rec.status
        );
    }
}
