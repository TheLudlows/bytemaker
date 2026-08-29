//! mcp — Model Context Protocol integration (s14).
//!
//! `McpManager` manages MCP server subprocesses: `connect` spawns → initialize →
//! list_tools → registers `McpTool` (prefixed `mcp__{server}__{tool}`) into the shared
//! `ToolRegistry`; `disconnect` / `shutdown_all` tear down. Exports `McpManager`,
//! `McpTool`, `McpClient`, `McpClientTrait`, and the `connect_mcp` / `disconnect_mcp` /
//! `list_mcp` tools.
//!
//! Known boundaries:
//! - Subprocess leak: `disconnect` returns `Err` when in-flight calls keep the Arc alive,
//!   but the connection is already removed; the process lives until those calls finish.
//! - `McpClient::shutdown` is a no-op; cleanup relies on `RunningService`'s Drop.
//! - `McpTool::check_permission` always returns `NeedsApproval` (v1).
//! - `connect_mcp` spawns arbitrary commands with no allowlist/sandbox.
//!
//! See `docs/modules/mcp.md`.

pub mod tool;
pub mod client;
pub mod tools;

pub use tool::{McpTool, McpClientTrait};
pub use client::{McpClient, InitResult};
pub use tools::{ConnectMcpTool, DisconnectMcpTool, ListMcpTool};

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use regex::Regex;
use crate::error::AgentError;
use crate::tools::registry::ToolRegistry;

const DISALLOWED_CHARS: &str = r"[^a-zA-Z0-9_-]";
const MAX_TOOL_NAME_LEN: usize = 64;

/// Replace characters not in [a-zA-Z0-9_-] with _
pub fn normalize_mcp_name(name: &str) -> Result<String, AgentError> {
    let re = Regex::new(DISALLOWED_CHARS).unwrap();
    let normalized = re.replace_all(name, "_").to_string();
    if normalized.is_empty() || normalized.chars().all(|c| c == '_') {
        return Err(AgentError::Validation("MCP name cannot normalize to empty".into()));
    }
    Ok(normalized)
}

/// Construct prefixed tool name: mcp__{server}__{tool}
pub fn prefixed_tool_name(server: &str, tool: &str) -> Result<String, AgentError> {
    let safe_server = normalize_mcp_name(server)?;
    let safe_tool = normalize_mcp_name(tool)?;
    let prefixed = format!("mcp__{}__{}", safe_server, safe_tool);
    if prefixed.len() > MAX_TOOL_NAME_LEN {
        return Err(AgentError::Validation(
            format!("MCP tool name exceeds {} chars: {}", MAX_TOOL_NAME_LEN, prefixed)
        ));
    }
    Ok(prefixed)
}

/// MCP tool definition from server (re-exported from client)
pub use client::McpToolDef;

/// Active MCP server connection
pub struct McpConnection {
    pub server_name: String,
    pub client: Arc<McpClient>,
    pub tools: Vec<McpToolDef>,
}

/// MCP server info for listing
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub tools: Vec<String>,
}

/// Manager for MCP server connections and tool registration
pub struct McpManager {
    connections: RwLock<HashMap<String, McpConnection>>,
    origins: RwLock<HashMap<String, String>>, // Normalized name -> origin (server/tool)
    registry: Arc<ToolRegistry>,
}

impl McpManager {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
            origins: RwLock::new(HashMap::new()),
            registry,
        }
    }

    /// Connect to MCP server, discover tools, register to registry.
    pub async fn connect(&self, server_name: &str, command: &str, args: &[&str]) -> Result<Vec<String>, AgentError> {
        // Spawn MCP client
        let client = McpClient::spawn(command, args).await?;

        // Initialize
        let _init = client.initialize().await?;

        // List tools
        let tools = client.list_tools().await?;

        let client = Arc::new(client);
        let mut registered_tool_names = Vec::new();

        for tool_def in &tools {
            // Generate prefixed name
            let prefixed = prefixed_tool_name(server_name, &tool_def.name)?;
            let origin = format!("{}/{}", server_name, tool_def.name);

            // Check for collision
            {
                let mut origins = self.origins.write().unwrap();
                if let Some(existing) = origins.get(&prefixed) {
                    return Err(AgentError::Validation(format!(
                        "MCP tool name collision: '{}' maps to both {} and {}",
                        prefixed, existing, origin
                    )));
                }
                origins.insert(prefixed.clone(), origin.clone());
            }

            // Create and register McpTool
            let mcp_tool = crate::mcp::tool::McpTool::new(
                prefixed.clone(),
                tool_def.name.clone(),
                server_name.to_string(),
                tool_def.description.clone(),
                tool_def.input_schema.clone(),
                Arc::clone(&client) as Arc<dyn crate::mcp::tool::McpClientTrait>,
            );

            self.registry.register_dynamic(Arc::new(mcp_tool));
            registered_tool_names.push(prefixed);
        }

        // Store connection
        {
            let mut connections = self.connections.write().unwrap();
            connections.insert(server_name.to_string(), McpConnection {
                server_name: server_name.to_string(),
                client,
                tools,
            });
        }

        Ok(registered_tool_names)
    }

    /// Disconnect MCP server, unregister tools.
    pub async fn disconnect(&self, server_name: &str) -> Result<(), AgentError> {
        let connection = {
            let mut connections = self.connections.write().unwrap();
            connections.remove(server_name)
                .ok_or_else(|| AgentError::Other(format!("MCP server '{}' not found", server_name)))?
        };

        // Unregister tools
        for tool_def in &connection.tools {
            let prefixed = prefixed_tool_name(server_name, &tool_def.name)?;
            self.registry.unregister(&prefixed);
            let mut origins = self.origins.write().unwrap();
            origins.remove(&prefixed);
        }

        // Shutdown client (kills child process)
        // Note: We can't unwrap the Arc since there may be other references
        // But we can let it be dropped when the connection is removed
        let _ = Arc::try_unwrap(connection.client)
            .map_err(|e| {
                tracing::warn!("Could not unwrap MCP client Arc (still in use): {}", e);
                AgentError::Other("MCP client still in use".into())
            })?
            .shutdown()
            .await;

        Ok(())
    }

    /// Force kill all MCP server processes.
    pub async fn shutdown_all(&self) {
        let connections: Vec<_> = {
            let mut conns = self.connections.write().unwrap();
            conns.drain().collect()
        };

        for (_name, connection) in connections {
            // Best effort shutdown - we're force killing anyway
            if let Ok(client) = Arc::try_unwrap(connection.client) {
                let _ = client.shutdown().await;
            }
        }
    }

    /// List all connected MCP servers
    pub fn list(&self) -> Vec<McpServerInfo> {
        let connections = self.connections.read().unwrap();
        connections.values()
            .map(|conn| McpServerInfo {
                name: conn.server_name.clone(),
                tools: conn.tools.iter().map(|t| t.name.clone()).collect(),
            })
            .collect()
    }

    /// Generate system prompt suffix (connected server list)
    pub fn system_prompt_suffix(&self) -> String {
        let servers = self.list();
        if servers.is_empty() {
            return String::new();
        }
        let mut lines = vec!["## Connected MCP Servers".to_string()];
        for server in servers {
            lines.push(format!("- {}: {} tool(s)", server.name, server.tools.len()));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod name_normalization_tests {
    use super::*;

    #[test]
    fn normalize_strips_dots_and_slashes() {
        assert_eq!(normalize_mcp_name("my.server").unwrap(), "my_server");
        assert_eq!(normalize_mcp_name("server/name").unwrap(), "server_name");
        assert_eq!(normalize_mcp_name("a.b/c-d_e.f").unwrap(), "a_b_c-d_e_f");
    }

    #[test]
    fn normalize_preserves_underscores_and_dashes() {
        assert_eq!(normalize_mcp_name("my_server").unwrap(), "my_server");
        assert_eq!(normalize_mcp_name("my-server").unwrap(), "my-server");
        assert_eq!(normalize_mcp_name("my_server-name").unwrap(), "my_server-name");
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(normalize_mcp_name("").is_err());
        assert!(normalize_mcp_name("...").is_err());
    }

    #[test]
    fn prefixed_name_format() {
        assert_eq!(prefixed_tool_name("docs", "search").unwrap(), "mcp__docs__search");
        assert_eq!(prefixed_tool_name("my-server", "my-tool").unwrap(), "mcp__my-server__my-tool");
    }

    #[test]
    fn prefixed_name_exceeds_64_chars() {
        let long_name = "a".repeat(100);
        let result = prefixed_tool_name(&long_name, "tool");
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("exceeds 64 chars"));
        }
    }

    #[test]
    fn mcp_tools_appear_in_tool_definitions() {
        // Registered MCP tools should appear in tool definitions.
        use crate::tools::registry::ToolRegistry;
        use crate::tools::trait_def::AgentKind;

        let registry = Arc::new(ToolRegistry::new());
        let manager = McpManager::new(Arc::clone(&registry));

        // Simulate MCP tool registration (call internal logic directly).
        let mcp_tool = crate::mcp::tool::McpTool::new(
            "mcp__test__search".to_string(),
            "search".to_string(),
            "test".to_string(),
            "Search the test server".to_string(),
            serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            Arc::new(crate::mcp::tool::MockMcpClient::new()),
        );

        manager.registry.register_dynamic(Arc::new(mcp_tool));

        // Verify tool appears in Lead definitions.
        let lead_defs = registry.definitions_for(AgentKind::Lead);
        assert!(lead_defs.iter().any(|d| d.name == "mcp__test__search"));

        // Verify tool appears in Subagent definitions.
        let subagent_defs = registry.definitions_for(AgentKind::Subagent);
        assert!(subagent_defs.iter().any(|d| d.name == "mcp__test__search"));

        // Verify tool appears in Teammate definitions.
        let teammate_defs = registry.definitions_for(AgentKind::Teammate);
        assert!(teammate_defs.iter().any(|d| d.name == "mcp__test__search"));
    }
}
