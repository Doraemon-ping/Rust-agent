//! File-oriented tools: `read_file`, `write_file`, `edit_file`, `glob`.

use std::fmt::Write as _;
use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{required_str, Tool, ToolContext};
use crate::error::{Error, Result};

/// Read a UTF-8 file, optionally limited to the first `limit` lines.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a text file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the workspace." },
                "limit": { "type": "integer", "description": "Return only the first N lines." }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let path = ctx.resolve(required_str(&input, "path")?)?;
        let content = tokio::fs::read_to_string(&path).await?;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        Ok(apply_line_limit(content, limit))
    }
}

/// Write content to a file, creating parent directories.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating parent directories as needed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let path = ctx.resolve(required_str(&input, "path")?)?;
        let content = required_str(&input, "content")?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        Ok(format!(
            "Wrote {} bytes to {}",
            content.len(),
            display_path(ctx, &path)
        ))
    }
}

/// Replace the first exact occurrence of `old_text` with `new_text`.
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace the first exact occurrence of old_text with new_text in a file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let path = ctx.resolve(required_str(&input, "path")?)?;
        let old_text = required_str(&input, "old_text")?;
        let new_text = required_str(&input, "new_text")?;

        let text = tokio::fs::read_to_string(&path).await?;
        if !text.contains(old_text) {
            return Err(Error::Tool {
                tool: "edit_file".into(),
                message: format!("text not found in {}", display_path(ctx, &path)),
            });
        }

        let updated = text.replacen(old_text, new_text, 1);
        tokio::fs::write(&path, updated).await?;
        Ok(format!("Edited {}", display_path(ctx, &path)))
    }
}

/// Find files matching a glob pattern (`**` matches recursively).
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern; `**` matches recursively."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. `src/**/*.rs`." }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let pattern = required_str(&input, "pattern")?;
        let root = ctx.workspace().display().to_string().replace('\\', "/");
        let full = format!(
            "{}/{}",
            root.trim_end_matches('/'),
            pattern.trim_start_matches('/')
        );

        let mut matches: Vec<String> = Vec::new();
        for entry in glob::glob(&full)? {
            let Ok(path) = entry else { continue };
            if path.starts_with(ctx.workspace()) {
                matches.push(display_path(ctx, &path));
            }
        }
        matches.sort();
        matches.dedup();

        if matches.is_empty() {
            return Ok("(no matches)".into());
        }

        let truncated = matches.len() > 200;
        matches.truncate(200);
        if truncated {
            matches.push("... (more matches omitted; narrow the pattern)".into());
        }
        Ok(matches.join("\n"))
    }
}

fn apply_line_limit(content: String, limit: Option<usize>) -> String {
    let Some(limit) = limit.filter(|n| *n > 0) else {
        return content;
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= limit {
        return content;
    }

    let omitted = lines.len() - limit;
    let mut out = lines[..limit].join("\n");
    let _ = write!(out, "\n... ({omitted} more lines)");
    out
}

fn display_path(ctx: &ToolContext, path: &Path) -> String {
    path.strip_prefix(ctx.workspace())
        .unwrap_or(path)
        .display()
        .to_string()
}
