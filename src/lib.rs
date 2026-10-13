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
pub mod cron;
pub mod error;
pub mod goal;
pub mod hooks;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod message;
pub mod permission;
pub mod recovery;
pub mod skills;
pub mod subagent;
pub mod team;
pub mod tools;
pub mod ui;
pub mod workflow;

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
pub use cron::{
    cron_matches, is_valid_cron_id, register_cron_tools, validate_cron, CancelCronTool, CronJob,
    CronScheduler, ListCronsTool, ScheduleCronTool, CANCEL_CRON_TOOL_NAME, CRON_PROMPT,
    LIST_CRONS_TOOL_NAME, SCHEDULED_TASKS_FILE, SCHEDULE_CRON_TOOL_NAME,
};
pub use error::{Error, Result};
pub use goal::{
    goal_continuation, parse_goal_command, parse_goal_evaluation, transcript_text, GoalCommand,
    GoalController, GoalDecision, GoalEvaluation, GoalEvaluator, GoalEvent, GoalListener,
    GoalState, PromptGoalEvaluator, CLEAR_ALIASES, DEFAULT_EVALUATOR_MAX_TOKENS,
    DEFAULT_STOP_HOOK_BLOCK_CAP, GOAL_EVALUATOR_MODEL_ENV, GOAL_EVALUATOR_SYSTEM, GOAL_PROMPT,
    MAX_GOAL_LENGTH, MAX_TRANSCRIPT_CHARS,
};
pub use hooks::{Hook, Hooks};
pub use llm::{
    AnthropicClient, CompletionRequest, CompletionResponse, LlmClient, StopReason, ToolDefinition,
    Usage,
};
pub use mcp::{
    available_servers, host_policy, normalize_mcp_name, prefixed_tool_name, register_mcp_tools,
    ConnectMcpTool, McpPermissionHook, McpPolicy, McpRegistry, McpServer, McpTool,
    CONNECT_MCP_TOOL_NAME, MCP_PROMPT, MCP_TOOL_PREFIX,
};
pub use memory::{
    build_system, memory_slug, should_store_memory, validate_memory_record, CatalogRecall, Memory,
    MemoryConfig, MemoryManager, MemoryOutcome, MemoryRecall, MemoryStore, MemoryType, Recalled,
};
pub use message::{ContentBlock, Message, Role, ToolCall};
pub use permission::{
    Approver, AutoApprover, Decision, DenyApprover, PermissionHook, PermissionPolicy,
};
pub use recovery::{
    classify, is_retryable, ErrorClass, RecoveryConfig, RecoveryState, CONTINUATION_PROMPT,
};
pub use skills::{parse_frontmatter, Skill, SkillLoader, SKILL_MANIFEST};
pub use subagent::{subagent_system_prompt, TaskTool, TASK_TOOL_NAME};
pub use team::{
    register_team_tools, register_teammate_tools, teammate_system_prompt, CreateWorktreeTool,
    ListTeammatesTool, MessageBus, PlanGate, RequestKind, RequestStatus, ReviewPlanTool,
    SendMessageTool, SpawnTeammateTool, SubmitPlanTool, Team, TeamMessage, TeammateStatus,
    CREATE_WORKTREE_TOOL_NAME, LEAD, LIST_TEAMMATES_TOOL_NAME, MAILBOXES_DIR,
    REQUEST_PLAN_TOOL_NAME, REQUEST_SHUTDOWN_TOOL_NAME, REVIEW_PLAN_TOOL_NAME,
    SEND_MESSAGE_TOOL_NAME, SPAWN_TEAMMATE_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME, TEAM_PROMPT,
    WORKTREES_DIR,
};
pub use tools::{
    register_task_tools, ClaimTaskTool, CompleteTaskTool, CreateTaskTool, GetTaskTool,
    ListTasksTool, LoadSkillTool, Task, TaskStatus, TaskStore, Tool, ToolContext, ToolRegistry,
    UpdateTaskTool, CLAIM_TASK_TOOL_NAME, COMPLETE_TASK_TOOL_NAME, CREATE_TASK_TOOL_NAME,
    DEFAULT_OWNER, GET_TASK_TOOL_NAME, LIST_TASKS_TOOL_NAME, LOAD_SKILL_TOOL_NAME, TASKS_DIR,
    TASKS_PROMPT, UPDATE_TASK_TOOL_NAME,
};
pub use workflow::{
    create_task_id, default_registry, fill_schema, findings_schema, last_run,
    register_workflow_tools, review_changes_meta, review_changes_script, stable_hash, valid_run_id,
    valid_workflow_name, validate_schema, verdict_schema, AgentRunner, AnthropicAgentRunner,
    BoxWorkflowFuture, Budget, ExecutionContext, ExecutionLimits, MockAgentRunner, PipelineStage,
    ProgressEvent, RunnerOutput, WorkflowEvent, WorkflowJournal, WorkflowListener, WorkflowMeta,
    WorkflowRegistry, WorkflowScript, WorkflowTask, WorkflowTool, AGENT_CAP, CONCURRENCY,
    DEMO_CHANGES, LAST_RUN_FILE, REVIEW_DIMENSIONS, WORKFLOWS_DIR, WORKFLOW_AGENT_MAX_TOKENS,
    WORKFLOW_AGENT_SYSTEM, WORKFLOW_PROMPT, WORKFLOW_TOOL_NAME,
};
