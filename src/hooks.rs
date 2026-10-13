//! Hook points: callbacks that run at fixed stages of the loop.
//!
//! This is the s04 idea. Instead of hard-coding policy inside the loop, the
//! loop calls hooks. Extension logic lives in hooks; the loop stays clean.
//!
//! | Event | When | Return value |
//! | --- | --- | --- |
//! | `UserPromptSubmit` | user submits a prompt | optional context to prepend |
//! | `PreToolUse` | before a tool runs | `Some` blocks the call |
//! | `PostToolUse` | after a tool runs | ignored (observe only) |
//! | `Stop` | model stopped asking for tools | `Some` injects a message and continues |

use std::sync::Arc;

use crate::message::{Message, ToolCall};

/// Callbacks at fixed points in the loop.
///
/// Every method has a no-op default, so implement only what you need.
pub trait Hook: Send + Sync {
    /// Runs when the user submits a prompt, before it reaches the model.
    ///
    /// Returning `Some(context)` prepends that context to the prompt.
    fn user_prompt_submit(&self, _prompt: &str) -> Option<String> {
        None
    }

    /// Runs before a tool is dispatched.
    ///
    /// Returning `Some(reason)` blocks the call; the reason becomes the tool
    /// result the model sees.
    fn pre_tool_use(&self, _call: &ToolCall) -> Option<String> {
        None
    }

    /// Runs after a tool produced output. Purely observational.
    fn post_tool_use(&self, _call: &ToolCall, _output: &str) {}

    /// Runs when the model stops asking for tools.
    ///
    /// Returning `Some(message)` appends that user message and keeps the loop
    /// going instead of returning.
    fn stop(&self, _history: &[Message]) -> Option<String> {
        None
    }
}

/// An ordered collection of hooks.
///
/// For the blocking events (`UserPromptSubmit`, `PreToolUse`, `Stop`) the
/// first hook to return `Some` wins, mirroring `trigger_hooks` in the
/// reference implementation.
#[derive(Default)]
pub struct Hooks {
    hooks: Vec<Arc<dyn Hook>>,
}

impl Hooks {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a hook.
    pub fn register(&mut self, hook: impl Hook + 'static) -> &mut Self {
        self.hooks.push(Arc::new(hook));
        self
    }

    /// Append an already-shared hook.
    pub fn register_arc(&mut self, hook: Arc<dyn Hook>) -> &mut Self {
        self.hooks.push(hook);
        self
    }

    /// Whether any hook is registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Run `UserPromptSubmit` hooks; the first `Some` wins.
    pub fn user_prompt_submit(&self, prompt: &str) -> Option<String> {
        self.hooks
            .iter()
            .find_map(|hook| hook.user_prompt_submit(prompt))
    }

    /// Run `PreToolUse` hooks; the first `Some` wins.
    pub fn pre_tool_use(&self, call: &ToolCall) -> Option<String> {
        self.hooks.iter().find_map(|hook| hook.pre_tool_use(call))
    }

    /// Run every `PostToolUse` hook, in order.
    pub fn post_tool_use(&self, call: &ToolCall, output: &str) {
        for hook in &self.hooks {
            hook.post_tool_use(call, output);
        }
    }

    /// Run `Stop` hooks; the first `Some` wins.
    pub fn stop(&self, history: &[Message]) -> Option<String> {
        self.hooks.iter().find_map(|hook| hook.stop(history))
    }
}
