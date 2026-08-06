// load_skill 工具 —— 按名称加载技能指令到上下文。
//
// 技能目录约定见 crate::skills：
//   <project>/.ueberneon/skills/<name>/SKILL.md
//   ~/.ueberneon/skills/<name>/SKILL.md

use std::path::PathBuf;

use crate::agent::{GenericsTool, ToolContext, ToolResult};
use crate::permission::Decision;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use schemars::JsonSchema;
use serde::Deserialize;
use ueberneon_macros::ToolMetaImpl;

/// load_skill — Load a skill's instructions into context by name.
/// Skills are directories containing a SKILL.md manifest, located in
/// `<project>/.ueberneon/skills/<name>/` or `~/.ueberneon/skills/<name>/`.
/// Returns the skill's instructions (frontmatter stripped).
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(argType = LoadSkillParams)]
pub struct LoadSkill {
    work_dir: PathBuf,
}

/// load_skill 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct LoadSkillParams {
    /// 技能名称（技能目录名）。
    #[schemars(description = "Skill name to load")]
    skill: String,
}

impl LoadSkill {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }

    async fn do_execute(
        &self,
        _ctx: &ToolContext,
        args: &LoadSkillParams,
    ) -> Result<ToolResult, String> {
        let name = args.skill.trim();
        if name.is_empty() {
            return Err("load_skill: missing 'skill' parameter".into());
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err("load_skill: invalid skill name".into());
        }

        let loaded = crate::skills::load(&self.work_dir, name)?;

        // 记录一次使用（技能不在注册表中时静默忽略）
        let _ = crate::db::with_db_result(|conn| {
            crate::db::metadata::skill::record_run_by_name(conn, name)
        });

        let mut out = String::new();
        out.push_str(&format!(
            "─── skill: {} — {}\n",
            loaded.name,
            loaded.path.display()
        ));
        if !loaded.description.is_empty() {
            out.push_str(&format!("description: {}\n", loaded.description));
        }
        out.push('\n');
        out.push_str(&loaded.instructions);
        Ok(ToolResult::ok(out))
    }
}

#[async_trait::async_trait]
impl GenericsTool for LoadSkill {
    async fn generics_execute(
        &self,
        ctx: &ToolContext,
        args: &LoadSkillParams,
    ) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for LoadSkill {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ActionMode, AgentHandler, Tool, ToolResultExt};
    use std::io::Write;

    #[tokio::test]
    async fn loads_skill_instructions() {
        let tmp = std::env::temp_dir().join(format!("_load_skill_test_{}", std::process::id()));
        let dir = tmp.join(".ueberneon").join("skills").join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("SKILL.md");
        let mut f = std::fs::File::create(&manifest).unwrap();
        f.write_all(
            b"---\nname: demo\ndescription: demo skill\ncategory: design\n---\n# demo\n\nfollow these steps.\n",
        )
        .unwrap();

        let tool = LoadSkill::new(tmp.clone());
        let result = tool
            .execute(
                &ToolContext {
                    call_id: "test".into(),
                    plan_mode: ActionMode::Regular,
                    handler: AgentHandler::default(),
                    progress: None,
                    main_conversation_id: String::new(),
                    project_id: None,
                    cancel_token: None,
                },
                &serde_json::json!({ "skill": "demo" }),
            )
            .await;

        assert!(result.error().is_none(), "error: {:?}", result.error());
        let output = result.output();
        assert!(output.contains("skill: demo"));
        assert!(output.contains("follow these steps."));
        assert!(!output.contains("frontmatter"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn rejects_invalid_names() {
        let tool = LoadSkill::new(std::env::temp_dir());
        let result = tool
            .execute(
                &ToolContext {
                    call_id: "test".into(),
                    plan_mode: ActionMode::Regular,
                    handler: AgentHandler::default(),
                    progress: None,
                    main_conversation_id: String::new(),
                    project_id: None,
                    cancel_token: None,
                },
                &serde_json::json!({ "skill": "../evil" }),
            )
            .await;
        assert!(result.is_err());
    }
}
