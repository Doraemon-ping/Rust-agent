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
//!
//! A workspace can add servers of its own: `plugins.json` declares them by
//! command, and [`McpRegistry::connect`] starts the child and speaks the same
//! `tools/list` / `tools/call` protocol to it over stdio.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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

/// Read a plugin's declared policy. Only `allow` runs without asking.
fn parse_policy(policy: Option<&str>) -> McpPolicy {
    match policy.map(str::trim) {
        Some("allow") => McpPolicy::Allow,
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

// ---------------------------------------------------------------------------
// Plugins declared by the workspace
// ---------------------------------------------------------------------------

/// The file a workspace declares its own servers in, beside `skills/`.
pub const PLUGIN_FILE: &str = "plugins.json";

/// One external server, as `<workspace>/plugins.json` declares it.
///
/// `command` is started with `args`; the server speaks the MCP JSON-RPC
/// protocol on its stdin and stdout, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSpec {
    /// Name the model connects it by and its tools are namespaced under.
    pub name: String,
    /// The program to start.
    pub command: String,
    /// Arguments handed to it.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child, on top of this process's own.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Host policy for its tools: `allow` runs them, `confirm` asks first.
    ///
    /// Defaults to `confirm`, which is the same answer [`host_policy`] gives a
    /// server it does not know.
    #[serde(default)]
    pub policy: Option<String>,
}

/// The file's own shape.
#[derive(Debug, Default, Deserialize)]
struct PluginFile {
    #[serde(default)]
    servers: Vec<PluginSpec>,
}

/// Read the plugins a workspace declares.
///
/// A missing file is not an error: a workspace with no plugins is the common
/// case. A file that cannot be parsed is, because silently ignoring it would
/// leave the page showing a list the workspace did not ask for.
pub fn load_plugin_specs(workspace: &Path) -> Result<Vec<PluginSpec>> {
    let path = workspace.join(PLUGIN_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let file: PluginFile = serde_json::from_str(&text)
        .map_err(|error| Error::Mcp(format!("{}: {error}", path.display())))?;
    for spec in &file.servers {
        if spec.name.trim().is_empty() {
            return Err(Error::Mcp(format!(
                "{}: a plugin needs a name",
                path.display()
            )));
        }
        if spec.command.trim().is_empty() {
            return Err(Error::Mcp(format!(
                "{}: plugin `{}` needs a command",
                path.display(),
                spec.name
            )));
        }
        if available_servers()
            .iter()
            .any(|builtin| *builtin == spec.name)
        {
            return Err(Error::Mcp(format!(
                "{}: plugin `{}` would shadow a built-in server",
                path.display(),
                spec.name
            )));
        }
    }
    Ok(file.servers)
}

/// How long a plugin has to answer the handshake, and each call after it.
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A server whose transport is a child process speaking JSON-RPC on stdio.
///
/// One thread reads the child's stdout and hands each answer to whoever is
/// waiting for that id, so a request can time out instead of hanging the turn
/// that made it. The child is killed when the server is dropped.
struct StdioServer {
    name: String,
    tools: Vec<McpTool>,
    stdin: Mutex<ChildStdin>,
    /// Answers waiting to be delivered, keyed by request id.
    waiting: Arc<Mutex<BTreeMap<u64, mpsc::Sender<Value>>>>,
    next_id: AtomicU64,
    /// Kept so the process is killed when this server goes away.
    _child: Mutex<Child>,
}

impl StdioServer {
    /// Start `spec` and read its `tools/list`.
    fn connect(spec: &PluginSpec) -> Result<StdioServer> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Do not flash a console window for a GUI-launched runtime.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|error| {
            Error::Mcp(format!(
                "plugin `{}` could not be started ({}): {error}",
                spec.name, spec.command
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Mcp(format!("plugin `{}` has no stdin", spec.name)))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Mcp(format!("plugin `{}` has no stdout", spec.name)))?;

        let waiting: Arc<Mutex<BTreeMap<u64, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        {
            let waiting = Arc::clone(&waiting);
            let name = spec.name.clone();
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(std::result::Result::ok) {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = message.get("id").and_then(Value::as_u64) else {
                        // A notification, or a malformed line: nothing waits.
                        continue;
                    };
                    let sender = waiting
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove(&id);
                    if let Some(sender) = sender {
                        let _ = sender.send(message);
                    }
                }
                let _ = name;
            });
        }

        let mut server = StdioServer {
            name: spec.name.clone(),
            tools: Vec::new(),
            stdin: Mutex::new(stdin),
            waiting,
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        };

        server.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "rust-agent", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        server.notify("notifications/initialized", json!({}))?;
        let listed = server.request("tools/list", json!({}))?;
        server.tools = parse_tool_list(&spec.name, &listed)?;

        Ok(server)
    }

    /// One request/response round trip, or a timeout naming the plugin.
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, sender);

        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        if let Err(error) = self.write_line(&line) {
            self.waiting
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&id);
            return Err(error);
        }

        match receiver.recv_timeout(PLUGIN_TIMEOUT) {
            Ok(message) => {
                if let Some(error) = message.get("error") {
                    return Err(Error::Mcp(format!(
                        "plugin `{}` refused {method}: {error}",
                        self.name
                    )));
                }
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            }
            Err(_) => {
                self.waiting
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&id);
                Err(Error::Mcp(format!(
                    "plugin `{}` did not answer {method} within {}s",
                    self.name,
                    PLUGIN_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// A one-way message: no id, no answer.
    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))?;
        self.write_line(&line)
    }

    fn write_line(&self, line: &str) -> Result<()> {
        let mut stdin = self.stdin.lock().unwrap_or_else(|error| error.into_inner());
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|error| {
                Error::Mcp(format!("plugin `{}` closed its stdin: {error}", self.name))
            })
    }
}

/// Read a `tools/list` result into the catalogue the registry works with.
fn parse_tool_list(server: &str, result: &Value) -> Result<Vec<McpTool>> {
    let Some(list) = result.get("tools").and_then(Value::as_array) else {
        return Err(Error::Mcp(format!(
            "plugin `{server}` answered tools/list without a tools array"
        )));
    };
    let mut tools = Vec::new();
    for entry in list {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let schema = entry
            .get("inputSchema")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        let mut tool = McpTool::new(name, description, schema);
        // The server's own claim, kept as a claim: the host policy decides.
        if entry
            .get("annotations")
            .and_then(|annotations| annotations.get("readOnlyHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            tool = tool.read_only();
        }
        tools.push(tool);
    }
    Ok(tools)
}

/// The text of a `tools/call` result, which is a list of content blocks.
fn tool_result_text(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            } else {
                parts.push(block.to_string());
            }
        }
    }
    if parts.is_empty() {
        return result.to_string();
    }
    parts.join("\n")
}

impl McpServer for StdioServer {
    fn name(&self) -> &str {
        &self.name
    }

    fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    fn call(&self, tool: &str, args: &Value) -> Result<String> {
        let result = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": args.clone() }),
        )?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(Error::Mcp(tool_result_text(&result)));
        }
        Ok(tool_result_text(&result))
    }
}

impl Drop for StdioServer {
    /// Take the child down with the server: an unconnected plugin should not
    /// outlive the runtime that started it.
    fn drop(&mut self) {
        if let Ok(mut child) = self._child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
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
    /// The servers the workspace declared in `plugins.json`.
    plugins: Mutex<Vec<PluginSpec>>,
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
                plugins: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Declare the servers a workspace listed in `plugins.json`.
    ///
    /// Declaration is not connection: a plugin is started only when someone
    /// connects it, by the page or by the model's `connect_mcp`.
    pub fn with_plugins(self, plugins: Vec<PluginSpec>) -> Self {
        *self
            .inner
            .plugins
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = plugins;
        self
    }

    /// The plugins this registry was given, in the order the file listed them.
    pub fn plugins(&self) -> Vec<PluginSpec> {
        self.inner
            .plugins
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// One declared plugin, by name.
    pub fn plugin(&self, name: &str) -> Option<PluginSpec> {
        self.plugins().into_iter().find(|spec| spec.name == name)
    }

    /// Every server that can be connected: the built-ins, then the declared
    /// plugins. This is the list the page shows and `connect_mcp` offers.
    pub fn available(&self) -> Vec<String> {
        let mut names: Vec<String> = available_servers()
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        for spec in self.plugins() {
            if !names.contains(&spec.name) {
                names.push(spec.name);
            }
        }
        names
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
            None => match self.plugin(name) {
                // A declared plugin is started here: connecting it is what
                // starts the process that advertises its tools.
                Some(spec) => Arc::new(StdioServer::connect(&spec)?) as Arc<dyn McpServer>,
                None => {
                    return Ok(format!(
                        "Unknown server '{name}'. Available: {}",
                        self.available().join(", ")
                    ))
                }
            },
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
                        self.policy_for_server(server_name, &tool.name),
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

    /// The policy for one server's tool.
    ///
    /// A plugin the workspace declared answers for itself, because the user who
    /// wrote `plugins.json` is the one who installed it. Everything else goes
    /// through [`host_policy`], which defaults to asking.
    fn policy_for_server(&self, server: &str, tool: &str) -> McpPolicy {
        match self.plugin(server) {
            Some(spec) => parse_policy(spec.policy.as_deref()),
            None => host_policy(server, tool),
        }
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
                "name": { "type": "string", "enum": self.registry.available() }
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
    interactive: bool,
}

impl McpPermissionHook {
    /// Wrap a registry and the approver used to answer a `confirm` policy.
    pub fn new(registry: McpRegistry, approver: Arc<dyn Approver>) -> Self {
        McpPermissionHook {
            registry,
            approver,
            interactive: true,
        }
    }

    /// Whether this turn may pause and ask the user (s15).
    ///
    /// A `confirm` tool in an asynchronous turn is denied rather than queued
    /// behind a prompt that will never be answered.
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }
}

impl Hook for McpPermissionHook {
    fn pre_tool_use(&self, call: &ToolCall) -> Option<String> {
        if !call.name.starts_with(MCP_TOOL_PREFIX) {
            return None;
        }
        match self.registry.policy_for(&call.name) {
            McpPolicy::Allow => None,
            McpPolicy::Confirm if !self.interactive => Some(format!(
                "Permission denied: external tool {} requires confirmation \
                 (interactive approval is unavailable during an asynchronous turn)",
                call.name
            )),
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

    #[test]
    fn a_workspace_plugin_file_names_the_servers_to_connect() {
        let dir = tempfile::tempdir().unwrap();
        // No file: no plugins, and no error about a file that is not there.
        assert!(load_plugin_specs(dir.path()).unwrap().is_empty());

        std::fs::write(
            dir.path().join(PLUGIN_FILE),
            r#"{
  "servers": [
    { "name": "notes", "command": "node", "args": ["notes.mjs"], "policy": "allow" },
    { "name": "tickets", "command": "tickets-mcp" }
  ]
}"#,
        )
        .unwrap();
        let specs = load_plugin_specs(dir.path()).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "notes");
        assert_eq!(specs[0].args, vec!["notes.mjs".to_string()]);
        assert_eq!(parse_policy(specs[0].policy.as_deref()), McpPolicy::Allow);
        // No policy means the same answer the host gives an unknown server.
        assert_eq!(parse_policy(specs[1].policy.as_deref()), McpPolicy::Confirm);
        assert_eq!(parse_policy(Some("whatever")), McpPolicy::Confirm);
    }

    #[test]
    fn a_plugin_file_that_cannot_be_used_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        for (case, body) in [
            ("not json", "{ oops"),
            ("no name", r#"{ "servers": [ { "command": "node" } ] }"#),
            ("no command", r#"{ "servers": [ { "name": "notes" } ] }"#),
            (
                "shadows a built-in",
                r#"{ "servers": [ { "name": "docs", "command": "node" } ] }"#,
            ),
        ] {
            std::fs::write(dir.path().join(PLUGIN_FILE), body).unwrap();
            assert!(
                load_plugin_specs(dir.path()).is_err(),
                "`{case}` was accepted"
            );
        }
    }

    #[test]
    fn declared_plugins_join_the_available_list_and_keep_their_own_policy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PLUGIN_FILE),
            r#"{ "servers": [
                { "name": "notes", "command": "node", "args": ["notes.mjs"], "policy": "allow" }
            ] }"#,
        )
        .unwrap();
        let registry = McpRegistry::new().with_plugins(load_plugin_specs(dir.path()).unwrap());

        // The built-ins are still there, and the declared server is offered
        // alongside them.
        assert_eq!(registry.available(), vec!["docs", "deploy", "notes"]);
        assert!(registry.plugin("notes").is_some());
        assert!(registry.plugin("nope").is_none());
        assert!(registry.connected_names().is_empty());

        // `allow` on the plugin is its own answer; a built-in still goes
        // through the host table.
        assert_eq!(
            registry.policy_for_server("notes", "anything"),
            McpPolicy::Allow
        );
        assert_eq!(
            registry.policy_for_server("docs", "search"),
            McpPolicy::Allow
        );
        assert_eq!(
            registry.policy_for_server("docs", "delete"),
            McpPolicy::Confirm
        );

        // A name no file declared and no build ships is still refused as a
        // string, not an error: the model asked, and the answer is information.
        assert!(registry
            .connect("ghost")
            .unwrap()
            .contains("Unknown server 'ghost'"));
    }

    #[test]
    fn a_plugin_that_cannot_start_reports_why() {
        let registry = McpRegistry::new().with_plugins(vec![PluginSpec {
            name: "broken".to_string(),
            command: "definitely-not-a-real-binary-9f2c".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            policy: None,
        }]);
        let error = registry.connect("broken").unwrap_err().to_string();
        assert!(error.contains("broken"), "{error}");
        assert!(!registry.is_connected("broken"));
    }
}
