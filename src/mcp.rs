//! s14: MCP -- discovering and calling external tools.
//!
//! The base tools are compiled in. External services are not: a *server*
//! advertises a `tools/list` catalogue plus a `tools/call` entry point, and the
//! harness decides what to do with it. This chapter splits the work in two:
//!
//! - [`McpServer`] is the server side: `tools()` and `call()`.
//! - [`McpRegistry`] is the host side: connecting, namespacing, the host
//!   policy, and merging the discovered tools into the model's tool pool.
//!
//! Discovered tools are namespaced `mcp__{server}__{tool}`, so two servers can
//! both offer `search` without colliding. Authorization comes from the host
//! policy table ([`host_policy`]), never from what a server claims about
//! itself -- a `readOnlyHint` in a description is a hint, not a permission.
//!
//! The shipped `docs` and `deploy` servers are in-process mocks: they show the
//! protocol boundary (`tools/list`, `tools/call`) without a real transport.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::hooks::Hook;
use crate::llm::ToolDefinition;
use crate::message::ToolCall;
use crate::permission::Approver;
use crate::tools::{required_str, Tool, ToolContext, ToolRegistry};

/// The namespace every discovered tool lives under.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// The tool that connects a server and discovers its tools.
pub const CONNECT_MCP_TOOL_NAME: &str = "connect_mcp";

/// The system-prompt nudge that tells the model how to reach external tools.
pub const MCP_PROMPT: &str =
    "External capabilities come from MCP servers. Call connect_mcp to connect one \
     (docs, deploy), then use the discovered `mcp__<server>__<tool>` tools; tools that \
     are not pre-approved will ask for confirmation.";

/// A function called when a server is connected, with its tool count.
pub type McpConnectListener = Arc<dyn Fn(&str, usize) + Send + Sync>;

/// Host-side authorization for a discovered external tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpPolicy {
    /// Run the tool without asking.
    Allow,
    /// Pause and ask the user before running.
    Confirm,
}

impl McpPolicy {
    /// The policy name, as it appears in configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            McpPolicy::Allow => "allow",
            McpPolicy::Confirm => "confirm",
        }
    }
}

/// The host's authorization table.
///
/// This is deliberately a host-side decision: a server cannot grant itself
/// permission by describing a tool as read-only. Anything not in the table
/// defaults to [`McpPolicy::Confirm`].
pub fn host_policy(server: &str, tool: &str) -> McpPolicy {
    match (server, tool) {
        ("docs", "search") | ("docs", "get_version") | ("deploy", "status") => McpPolicy::Allow,
        _ => McpPolicy::Confirm,
    }
}

/// A tool advertised by an MCP server.
#[derive(Debug, Clone)]
pub struct McpTool {
    /// The server's own tool name, before namespacing.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the arguments.
    pub input_schema: Value,
    /// The server's claim that the tool only reads. Advisory, never trusted.
    pub read_only_hint: bool,
    /// The server's claim that the tool destroys data. Advisory, never trusted.
    pub destructive_hint: bool,
}

impl McpTool {
    /// A tool with no annotations.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        McpTool {
            name: name.into(),
            description: description.into(),
            input_schema,
            read_only_hint: false,
            destructive_hint: false,
        }
    }

    /// Mark the tool as read-only, as the server claims.
    pub fn read_only(mut self) -> Self {
        self.read_only_hint = true;
        self
    }

    /// Mark the tool as destructive, as the server claims.
    pub fn destructive(mut self) -> Self {
        self.destructive_hint = true;
        self
    }
}

/// The server side: what a connected service offers and how to call it.
pub trait McpServer: Send + Sync {
    /// The server's name, used to namespace its tools.
    fn name(&self) -> &str;
    /// The `tools/list` result.
    fn tools(&self) -> &[McpTool];
    /// The `tools/call` entry point. Errors are surfaced to the model.
    fn call(&self, tool: &str, args: &Value) -> Result<String>;
}

/// Replace characters outside the model tool-name alphabet with `_`.
///
/// Model tool names allow letters, digits, `_`, and `-`. A dotted or slashed
/// server name (`docs.one/get.version`) would otherwise be unusable.
pub fn normalize_mcp_name(name: &str) -> Result<String> {
    let normalized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if normalized.is_empty() {
        return Err(Error::Mcp(
            "MCP names cannot normalize to an empty string".to_string(),
        ));
    }
    Ok(normalized)
}

/// The namespaced name the model sees: `mcp__{server}__{tool}`.
///
/// Both halves are normalized first, and the result is capped at the 64
/// characters model tool names allow.
pub fn prefixed_tool_name(server: &str, tool: &str) -> Result<String> {
    let safe_server = normalize_mcp_name(server)?;
    let safe_tool = normalize_mcp_name(tool)?;
    let prefixed = format!("{MCP_TOOL_PREFIX}{safe_server}__{safe_tool}");
    if prefixed.len() > 64 {
        return Err(Error::Mcp(format!(
            "MCP tool name is longer than 64 characters: {prefixed}"
        )));
    }
    Ok(prefixed)
}

/// Builds a fresh server instance.
type ServerFactory = fn() -> Arc<dyn McpServer>;

/// A connection factory, keyed by server name.
const MOCK_SERVERS: &[(&str, ServerFactory)] = &[
    ("docs", || Arc::new(DocsServer::new())),
    ("deploy", || Arc::new(DeployServer::new())),
];

/// The servers [`McpRegistry::connect`] knows how to reach.
pub fn available_servers() -> Vec<&'static str> {
    MOCK_SERVERS.iter().map(|(name, _)| *name).collect()
}

fn build_server(name: &str) -> Option<Arc<dyn McpServer>> {
    MOCK_SERVERS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, factory)| factory())
}

/// `docs`: a small documentation service.
struct DocsServer {
    tools: Vec<McpTool>,
}

impl DocsServer {
    fn new() -> Self {
        DocsServer {
            tools: vec![
                McpTool::new(
                    "search",
                    "Search the documentation.",
                    json!({
                        "type": "object",
                        "properties": { "query": { "type": "string" } },
                        "required": ["query"],
                    }),
                )
                .read_only(),
                McpTool::new(
                    "get_version",
                    "Get the documentation API version.",
                    json!({ "type": "object", "properties": {} }),
                )
                .read_only(),
            ],
        }
    }
}

impl McpServer for DocsServer {
    fn name(&self) -> &str {
        "docs"
    }

    fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    fn call(&self, tool: &str, args: &Value) -> Result<String> {
        match tool {
            "search" => {
                let query = required_str(args, "query")?;
                Ok(format!("[docs] Found 3 results for '{query}'"))
            }
            "get_version" => Ok("[docs] API v2.1.0".to_string()),
            other => Err(Error::Mcp(format!("unknown tool '{other}'"))),
        }
    }
}

/// `deploy`: a deployment platform with a destructive `trigger`.
struct DeployServer {
    tools: Vec<McpTool>,
}

impl DeployServer {
    fn new() -> Self {
        DeployServer {
            tools: vec![
                McpTool::new(
                    "trigger",
                    "Trigger a deployment.",
                    json!({
                        "type": "object",
                        "properties": { "service": { "type": "string" } },
                        "required": ["service"],
                    }),
                )
                .destructive(),
                McpTool::new(
                    "status",
                    "Check deployment status.",
                    json!({
                        "type": "object",
                        "properties": { "service": { "type": "string" } },
                        "required": ["service"],
                    }),
                )
                .read_only(),
            ],
        }
    }
}

impl McpServer for DeployServer {
    fn name(&self) -> &str {
        "deploy"
    }

    fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    fn call(&self, tool: &str, args: &Value) -> Result<String> {
        match tool {
            "trigger" => {
                let service = required_str(args, "service")?;
                Ok(format!("[deploy] Triggered: {service}"))
            }
            "status" => {
                let service = required_str(args, "service")?;
                Ok(format!("[deploy] {service}: running (v1.4.2)"))
            }
            other => Err(Error::Mcp(format!("unknown tool '{other}'"))),
        }
    }
}

/// The set of connected servers, plus the host policy and a connect listener.
///
/// Cheap to clone: every clone shares one registry, so the `connect_mcp` tool,
/// the tool pool, and the permission hook all see the same connections.
#[derive(Clone)]
pub struct McpRegistry {
    inner: Arc<McpInner>,
}

struct McpInner {
    servers: Mutex<BTreeMap<String, Arc<dyn McpServer>>>,
    listener: Mutex<Option<McpConnectListener>>,
}

impl Default for McpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl McpRegistry {
    /// An empty registry: no servers connected.
    pub fn new() -> Self {
        McpRegistry {
            inner: Arc::new(McpInner {
                servers: Mutex::new(BTreeMap::new()),
                listener: Mutex::new(None),
            }),
        }
    }

    /// Report every connection through `listener`.
    pub fn with_listener(self, listener: McpConnectListener) -> Self {
        self.set_listener(listener);
        self
    }

    /// Set (or replace) the connect listener.
    pub fn set_listener(&self, listener: McpConnectListener) {
        *self.inner.listener.lock().unwrap() = Some(listener);
    }

    /// Whether a server is connected.
    pub fn is_connected(&self, name: &str) -> bool {
        self.inner.servers.lock().unwrap().contains_key(name)
    }

    /// The connected server names, in stable order.
    pub fn connected_names(&self) -> Vec<String> {
        self.inner.servers.lock().unwrap().keys().cloned().collect()
    }

    /// Connect a known server and discover its tools.
    ///
    /// Unknown names and repeat connections are reported back as ordinary
    /// strings, not errors: the model asked, and the answer is information.
    pub fn connect(&self, name: &str) -> Result<String> {
        if self.is_connected(name) {
            return Ok(format!("MCP server '{name}' already connected"));
        }
        let server = match build_server(name) {
            Some(server) => server,
            None => {
                return Ok(format!(
                    "Unknown server '{name}'. Available: {}",
                    available_servers().join(", ")
                ))
            }
        };
        let tool_count = server.tools().len();
        self.register_server(Arc::clone(&server))?;
        if let Some(listener) = self.inner.listener.lock().unwrap().as_ref() {
            listener(name, tool_count);
        }
        let names: Vec<&str> = server
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        Ok(format!(
            "Connected to MCP server '{name}'. Discovered {tool_count} tools: {}",
            names.join(", ")
        ))
    }

    /// Insert a server directly.
    ///
    /// This is the seam for a real transport: build an [`McpServer`] over your
    /// transport and register it, and everything downstream is unchanged.
    /// Malformed catalogs and name collisions are rejected here, once, so the
    /// per-turn tool pool can never half-fail.
    pub fn register_server(&self, server: Arc<dyn McpServer>) -> Result<()> {
        let name = server.name().to_string();
        let mut servers = self.inner.servers.lock().unwrap();
        if servers.contains_key(&name) {
            return Err(Error::Mcp(format!(
                "MCP server '{name}' is already connected"
            )));
        }
        let mut local = Vec::new();
        for tool in server.tools() {
            let prefixed = prefixed_tool_name(&name, &tool.name)?;
            if !tool.input_schema.is_object() {
                return Err(Error::Mcp(format!(
                    "Invalid input schema for MCP tool '{name}/{}'",
                    tool.name
                )));
            }
            if let Some(kind) = tool.input_schema.get("type").and_then(Value::as_str) {
                if kind != "object" {
                    return Err(Error::Mcp(format!(
                        "Invalid input schema for MCP tool '{name}/{}'",
                        tool.name
                    )));
                }
            }
            if local.contains(&prefixed) {
                return Err(Error::Mcp(format!(
                    "Duplicate MCP tool name on server '{name}': '{prefixed}'"
                )));
            }
            for (other_name, other) in servers.iter() {
                for other_tool in other.tools() {
                    if prefixed_tool_name(other_name, &other_tool.name)? == prefixed {
                        return Err(Error::Mcp(format!(
                            "MCP tool name collision after normalization: '{prefixed}' \
                             maps both '{other_name}/{}' and '{name}/{}'",
                            other_tool.name, tool.name
                        )));
                    }
                }
            }
            local.push(prefixed);
        }
        servers.insert(name, server);
        Ok(())
    }

    /// The discovered tools, namespaced and ready for the model.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let servers = self.inner.servers.lock().unwrap();
        let mut definitions = Vec::new();
        for (server_name, server) in servers.iter() {
            for tool in server.tools() {
                let Ok(prefixed) = prefixed_tool_name(server_name, &tool.name) else {
                    continue;
                };
                definitions.push(ToolDefinition {
                    name: prefixed,
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                });
            }
        }
        definitions
    }

    /// Resolve a namespaced name to `(server, server name, raw tool, policy)`.
    pub fn resolve(&self, name: &str) -> Option<(Arc<dyn McpServer>, String, String, McpPolicy)> {
        let servers = self.inner.servers.lock().unwrap();
        for (server_name, server) in servers.iter() {
            for tool in server.tools() {
                if prefixed_tool_name(server_name, &tool.name).ok().as_deref() == Some(name) {
                    return Some((
                        Arc::clone(server),
                        server_name.clone(),
                        tool.name.clone(),
                        host_policy(server_name, &tool.name),
                    ));
                }
            }
        }
        None
    }

    /// The host policy for a namespaced tool. Unknown names require confirmation.
    pub fn policy_for(&self, name: &str) -> McpPolicy {
        self.resolve(name)
            .map(|(_, _, _, policy)| policy)
            .unwrap_or(McpPolicy::Confirm)
    }

    /// Call a discovered tool. The raw server name is recovered from the
    /// namespaced one, so the server never sees the `mcp__` prefix.
    pub fn call_tool(&self, name: &str, args: &Value) -> Result<String> {
        match self.resolve(name) {
            Some((server, _, tool, _)) => server.call(&tool, args),
            None => Err(Error::UnknownTool(name.to_string())),
        }
    }

    /// The line appended to the system prompt, listing connected servers.
    pub fn prompt_section(&self) -> String {
        let names = self.connected_names();
        if names.is_empty() {
            String::new()
        } else {
            format!("\n\nConnected MCP servers: {}", names.join(", "))
        }
    }
}

/// The `connect_mcp` tool: connect a server, discover its tools.
///
/// The discovered tools join the pool on the *next* model call, which is what
/// makes the pool dynamic. Two rounds after this returns, the model can see
/// `mcp__docs__search` and friends.
pub struct ConnectMcpTool {
    registry: McpRegistry,
}

impl ConnectMcpTool {
    /// Wrap a registry.
    pub fn new(registry: McpRegistry) -> Self {
        ConnectMcpTool { registry }
    }
}

#[async_trait]
impl Tool for ConnectMcpTool {
    fn name(&self) -> &str {
        CONNECT_MCP_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Connect to an MCP server and discover its tools. After connecting, \
         the server's tools join the pool as mcp__<server>__<tool>."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "enum": available_servers() }
            },
            "required": ["name"],
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let name = required_str(&input, "name")?;
        self.registry.connect(name)
    }
}

/// Register the MCP tools (currently just `connect_mcp`).
pub fn register_mcp_tools(registry: &mut ToolRegistry, mcp: McpRegistry) {
    registry.register(ConnectMcpTool::new(mcp));
}

/// Enforces the host policy for `mcp__*` tools, as a `PreToolUse` hook.
///
/// A tool the host policy marks `allow` runs straight away; anything else
/// pauses at the approver, exactly like a destructive shell command. The
/// server's own annotations are ignored here on purpose.
pub struct McpPermissionHook {
    registry: McpRegistry,
    approver: Arc<dyn Approver>,
}

impl McpPermissionHook {
    /// Wrap a registry and the approver used to answer a `confirm` policy.
    pub fn new(registry: McpRegistry, approver: Arc<dyn Approver>) -> Self {
        McpPermissionHook { registry, approver }
    }
}

impl Hook for McpPermissionHook {
    fn pre_tool_use(&self, call: &ToolCall) -> Option<String> {
        if !call.name.starts_with(MCP_TOOL_PREFIX) {
            return None;
        }
        match self.registry.policy_for(&call.name) {
            McpPolicy::Allow => None,
            McpPolicy::Confirm => {
                let reason = format!("external tool {} requires confirmation", call.name);
                if self.approver.approve(call, &reason) {
                    None
                } else {
                    Some(format!("Permission denied by user: {reason}"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::DenyApprover;

    fn context() -> ToolContext {
        ToolContext::new(".")
    }

    fn call(id: &str, name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    #[test]
    fn normalize_replaces_disallowed_characters() {
        assert_eq!(
            normalize_mcp_name("docs.one/get.version").unwrap(),
            "docs_one_get_version"
        );
        assert_eq!(normalize_mcp_name("docs-2").unwrap(), "docs-2");
        assert!(normalize_mcp_name("").is_err());
        assert!(normalize_mcp_name("***").is_ok());
    }

    #[test]
    fn prefixed_name_uses_the_namespace() {
        assert_eq!(
            prefixed_tool_name("docs", "search").unwrap(),
            "mcp__docs__search"
        );
        let long = "x".repeat(80);
        assert!(prefixed_tool_name(&long, "t").is_err());
    }

    #[test]
    fn connect_discovers_tools() {
        let registry = McpRegistry::new();
        assert!(registry.tool_definitions().is_empty());

        let message = registry.connect("docs").unwrap();
        assert!(message.contains("Discovered 2 tools"));
        assert!(registry.is_connected("docs"));

        let names: Vec<String> = registry
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        // Tools keep the server's declared order; servers are visited in name order.
        assert_eq!(names, vec!["mcp__docs__search", "mcp__docs__get_version"]);
    }

    #[test]
    fn connect_is_idempotent_and_reports_unknown_servers() {
        let registry = McpRegistry::new();
        registry.connect("docs").unwrap();
        let again = registry.connect("docs").unwrap();
        assert!(again.contains("already connected"));
        let unknown = registry.connect("nope").unwrap();
        assert!(unknown.contains("Unknown server 'nope'"));
        assert!(unknown.contains("docs, deploy"));
    }

    #[test]
    fn connect_notifies_the_listener() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let registry =
            McpRegistry::new().with_listener(Arc::new(move |name: &str, tools: usize| {
                sink.lock().unwrap().push((name.to_string(), tools));
            }));
        registry.connect("deploy").unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![("deploy".to_string(), 2)]);
    }

    #[tokio::test]
    async fn connect_tool_registers_discovered_tools() {
        let registry = McpRegistry::new();
        let mut tools = ToolRegistry::new();
        register_mcp_tools(&mut tools, registry.clone());

        let message = tools
            .execute(
                &context(),
                &call("1", "connect_mcp", json!({ "name": "docs" })),
            )
            .await
            .unwrap();
        assert!(message.contains("docs"));
        assert_eq!(registry.connected_names(), vec!["docs".to_string()]);
    }

    #[test]
    fn call_tool_dispatches_to_the_server() {
        let registry = McpRegistry::new();
        registry.connect("docs").unwrap();
        registry.connect("deploy").unwrap();
        let output = registry
            .call_tool("mcp__docs__search", &json!({ "query": "agent hooks" }))
            .unwrap();
        assert_eq!(output, "[docs] Found 3 results for 'agent hooks'");
        let output = registry
            .call_tool("mcp__deploy__status", &json!({ "service": "web" }))
            .unwrap();
        assert_eq!(output, "[deploy] web: running (v1.4.2)");
    }

    #[test]
    fn call_tool_errors_stay_at_the_tool_boundary() {
        let registry = McpRegistry::new();
        registry.connect("docs").unwrap();
        // Missing argument: the server reports it, the loop keeps going.
        let error = registry
            .call_tool("mcp__docs__search", &json!({}))
            .unwrap_err();
        assert!(matches!(error, Error::Tool { .. }));
        // Unknown namespaced tool.
        assert!(matches!(
            registry.call_tool("mcp__docs__missing", &json!({})),
            Err(Error::UnknownTool(_))
        ));
    }

    #[test]
    fn host_policy_is_not_taken_from_the_server() {
        assert_eq!(host_policy("docs", "search"), McpPolicy::Allow);
        assert_eq!(host_policy("deploy", "status"), McpPolicy::Allow);
        // Destructive by server hint, but the host policy decides.
        assert_eq!(host_policy("deploy", "trigger"), McpPolicy::Confirm);
        assert_eq!(host_policy("docs", "unknown"), McpPolicy::Confirm);

        let registry = McpRegistry::new();
        registry.connect("deploy").unwrap();
        assert_eq!(registry.policy_for("mcp__deploy__status"), McpPolicy::Allow);
        assert_eq!(
            registry.policy_for("mcp__deploy__trigger"),
            McpPolicy::Confirm
        );
        assert_eq!(registry.policy_for("mcp__nope__x"), McpPolicy::Confirm);
    }

    #[test]
    fn permission_hook_gates_confirm_tools() {
        let registry = McpRegistry::new();
        registry.connect("deploy").unwrap();
        let hook = McpPermissionHook::new(registry, Arc::new(DenyApprover));

        // `allow` policy: no hook output.
        assert!(hook
            .pre_tool_use(&call("1", "mcp__deploy__status", json!({})))
            .is_none());
        // `confirm` policy + refusing approver: blocked.
        let blocked = hook
            .pre_tool_use(&call("2", "mcp__deploy__trigger", json!({})))
            .unwrap();
        assert!(blocked.contains("Permission denied"));
        // Non-MCP tools are left alone.
        assert!(hook.pre_tool_use(&call("3", "bash", json!({}))).is_none());
    }

    #[test]
    fn prompt_section_lists_connected_servers() {
        let registry = McpRegistry::new();
        assert_eq!(registry.prompt_section(), "");
        registry.connect("docs").unwrap();
        registry.connect("deploy").unwrap();
        assert_eq!(
            registry.prompt_section(),
            "\n\nConnected MCP servers: deploy, docs"
        );
    }

    #[test]
    fn register_server_rejects_collisions_and_bad_schemas() {
        struct Fixed {
            name: String,
            tools: Vec<McpTool>,
        }
        impl McpServer for Fixed {
            fn name(&self) -> &str {
                &self.name
            }
            fn tools(&self) -> &[McpTool] {
                &self.tools
            }
            fn call(&self, _tool: &str, _args: &Value) -> Result<String> {
                Ok(String::new())
            }
        }

        let registry = McpRegistry::new();
        // `a.b` and `a_b` normalize to the same namespace.
        registry
            .register_server(Arc::new(Fixed {
                name: "a.b".to_string(),
                tools: vec![McpTool::new("t", "d", json!({ "type": "object" }))],
            }))
            .unwrap();
        let collision = registry.register_server(Arc::new(Fixed {
            name: "a_b".to_string(),
            tools: vec![McpTool::new("t", "d", json!({ "type": "object" }))],
        }));
        assert!(collision.is_err());

        let bad_schema = registry.register_server(Arc::new(Fixed {
            name: "b".to_string(),
            tools: vec![McpTool::new("t", "d", json!({ "type": "array" }))],
        }));
        assert!(bad_schema.is_err());
    }
}
