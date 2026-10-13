//! The `load_skill` tool (s07): read a full `SKILL.md` on demand.
//!
//! The system prompt only lists skill names and descriptions. This tool is how
//! the model pulls the full instructions once a task matches a skill.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{required_str, Tool, ToolContext};
use crate::error::Result;
use crate::skills::SkillLoader;

/// Tool name the model calls to load a skill's full content.
pub const LOAD_SKILL_TOOL_NAME: &str = "load_skill";

/// Resolves skill names against a shared [`SkillLoader`].
pub struct LoadSkillTool {
    loader: Arc<SkillLoader>,
}

impl LoadSkillTool {
    /// Wrap a loader.
    pub fn new(loader: Arc<SkillLoader>) -> Self {
        LoadSkillTool { loader }
    }

    /// The loader this tool reads from.
    pub fn loader(&self) -> &SkillLoader {
        &self.loader
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        LOAD_SKILL_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Load the full SKILL.md content by skill name. Call this when a task matches one of the \
         skills listed in the system prompt."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Skill name, as shown in the system prompt catalog."
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let name = required_str(&input, "name")?;
        Ok(self.loader.load(name)?.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;
    use crate::skills::SKILL_MANIFEST;
    use crate::tools::ToolRegistry;

    fn demo_loader() -> (tempfile::TempDir, Arc<SkillLoader>) {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join(SKILL_MANIFEST),
            "---\nname: demo\ndescription: Demo.\n---\n\nFull instructions.",
        )
        .unwrap();
        let loader = Arc::new(SkillLoader::new(dir.path()));
        (dir, loader)
    }

    #[tokio::test]
    async fn call_returns_the_full_manifest() {
        let (dir, loader) = demo_loader();
        let tool = LoadSkillTool::new(loader);
        let ctx = ToolContext::new(dir.path());
        let output = tool.call(&ctx, json!({ "name": "demo" })).await.unwrap();
        assert!(output.contains("Full instructions."), "{output}");
    }

    #[tokio::test]
    async fn unknown_skill_is_an_error() {
        let (dir, loader) = demo_loader();
        let tool = LoadSkillTool::new(loader);
        let ctx = ToolContext::new(dir.path());
        let error = tool
            .call(&ctx, json!({ "name": "missing" }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown skill"), "{error}");
    }

    #[tokio::test]
    async fn registry_dispatches_load_skill_by_name() {
        let (dir, loader) = demo_loader();
        let mut registry = ToolRegistry::new();
        registry.register(LoadSkillTool::new(loader));

        let ctx = ToolContext::new(dir.path());
        let call = ToolCall {
            id: "call_1".into(),
            name: LOAD_SKILL_TOOL_NAME.into(),
            input: json!({ "name": "demo" }),
        };
        let output = registry.execute(&ctx, &call).await.unwrap();
        assert!(output.contains("Full instructions."), "{output}");
    }
}
