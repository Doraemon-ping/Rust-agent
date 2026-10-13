//! A minimal, extensible coding-agent runtime in Rust.
//!
//! The whole idea fits in one loop: call the model, run whatever tools it
//! asks for, feed the results back, and repeat until it stops asking.
//! Everything else -- tools, permissions, the provider -- is a seam you can
//! replace without touching the loop.
//!
//! ```no_run
//! use rust_agent::{Agent, AnthropicClient, Config};
//!
//! # async fn demo() -> rust_agent::Result<()> {
//! dotenvy::dotenv().ok();
//! let client = AnthropicClient::new(&Config::from_env()?);
//! let agent = Agent::builder().client(client).build();
//! let answer = agent.ask("list the Rust files in this directory").await?;
//! println!("{answer}");
//! # Ok(())
//! # }
//! ```

pub mod agent;
pub mod background;
pub mod compact;
pub mod config;
pub mod error;
pub mod hooks;
pub mod llm;
pub mod memory;
pub mod message;
pub mod permission;
pub mod skills;
pub mod subagent;
pub mod tools;

pub use agent::{
    default_system_prompt, Agent, AgentBuilder, AgentEvent, AgentObserver, NoopObserver,
};
pub use background::{
    placeholder, should_run_background, BackgroundManager, BACKGROUND_PROMPT, NOTIFICATION_TAG,
};
pub use compact::{
    is_context_overflow, CompactConfig, CompactTool, CompactionListener, Compactor,
    COMPACT_TOOL_NAME,
};
pub use config::Config;
pub use error::{Error, Result};
pub use hooks::{Hook, Hooks};
pub use llm::{
    AnthropicClient, CompletionRequest, CompletionResponse, LlmClient, StopReason, ToolDefinition,
    Usage,
};
pub use memory::{
    build_system, memory_slug, should_store_memory, validate_memory_record, CatalogRecall, Memory,
    MemoryConfig, MemoryManager, MemoryOutcome, MemoryRecall, MemoryStore, MemoryType, Recalled,
};
pub use message::{ContentBlock, Message, Role, ToolCall};
pub use permission::{
    Approver, AutoApprover, Decision, DenyApprover, PermissionHook, PermissionPolicy,
};
pub use skills::{parse_frontmatter, Skill, SkillLoader, SKILL_MANIFEST};
pub use subagent::{subagent_system_prompt, TaskTool, TASK_TOOL_NAME};
pub use tools::{
    register_task_tools, ClaimTaskTool, CompleteTaskTool, CreateTaskTool, GetTaskTool,
    ListTasksTool, LoadSkillTool, Task, TaskStatus, TaskStore, Tool, ToolContext, ToolRegistry,
    UpdateTaskTool, CLAIM_TASK_TOOL_NAME, COMPLETE_TASK_TOOL_NAME, CREATE_TASK_TOOL_NAME,
    DEFAULT_OWNER, GET_TASK_TOOL_NAME, LIST_TASKS_TOOL_NAME, LOAD_SKILL_TOOL_NAME, TASKS_DIR,
    TASKS_PROMPT, UPDATE_TASK_TOOL_NAME,
};
