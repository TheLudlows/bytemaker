/*
mcp/tools.rs - MCP Management Tools

This module implements three tools for managing MCP server connections:
- ConnectMcpTool: Connect to an MCP server and discover its tools
- DisconnectMcpTool: Disconnect from an MCP server
- ListMcpTool: List all connected MCP servers and their tools
*/

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::Value;
use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
use super::McpManager;

/// Connect to an MCP server and discover its tools.
/// Lead-only to prevent subagents from modifying the tool pool.
pub struct ConnectMcpTool {
    manager: Arc<McpManager>,
}

impl ConnectMcpTool {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ConnectMcpTool {
    fn name(&self) -> &str {
        "connect_mcp"
    }

    fn description(&self) -> &str {
        "Connect to an MCP server and discover its tools. Discovered tools become available as mcp__{server}__{tool}."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Server name (used for tool namespacing)"
                },
                "command": {
                    "type": "string",
                    "description": "Command to start the MCP server (e.g., 'npx')"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Arguments to the command (e.g., ['-y', '@modelcontextprotocol/server-docs'])"
                }
            },
            "required": ["name", "command"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        // Process spawning requires user confirmation
        PermissionCheck::NeedsApproval("Connecting to an external MCP server will spawn a new process")
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let args = input.get("args")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        if name.is_empty() || command.is_empty() {
            return "Error: 'name' and 'command' are required parameters".to_string();
        }

        match self.manager.connect(name, command, &args).await {
            Ok(tools) => {
                if tools.is_empty() {
                    format!("Connected to MCP server '{}'. No tools discovered.", name)
                } else {
                    format!(
                        "Connected to MCP server '{}'. Discovered {} tool(s):\n{}",
                        name,
                        tools.len(),
                        tools.join(", ")
                    )
                }
            }
            Err(e) => format!("Error connecting to MCP server '{}': {}", name, e),
        }
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        // Only Lead can connect to MCP servers
        kind == AgentKind::Lead
    }
}

/// Disconnect from an MCP server and remove its tools.
/// Lead-only to prevent subagents from modifying the tool pool.
pub struct DisconnectMcpTool {
    manager: Arc<McpManager>,
}

impl DisconnectMcpTool {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for DisconnectMcpTool {
    fn name(&self) -> &str {
        "disconnect_mcp"
    }

    fn description(&self) -> &str {
        "Disconnect from an MCP server and remove its tools from the registry."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Server name to disconnect"
                }
            },
            "required": ["name"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        // Process termination requires user confirmation
        PermissionCheck::NeedsApproval("Disconnecting from an MCP server will terminate its process")
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");

        if name.is_empty() {
            return "Error: 'name' is a required parameter".to_string();
        }

        match self.manager.disconnect(name).await {
            Ok(()) => format!("Disconnected from MCP server '{}'", name),
            Err(e) => format!("Error disconnecting from MCP server '{}': {}", name, e),
        }
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        // Only Lead can disconnect from MCP servers
        kind == AgentKind::Lead
    }
}

/// List all connected MCP servers and their discovered tools.
/// Available to all agents for discovery purposes.
pub struct ListMcpTool {
    manager: Arc<McpManager>,
}

impl ListMcpTool {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ListMcpTool {
    fn name(&self) -> &str {
        "list_mcp"
    }

    fn description(&self) -> &str {
        "List all connected MCP servers and their discovered tools."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
        let servers = self.manager.list();
        if servers.is_empty() {
            return "No MCP servers connected. Use connect_mcp to connect one.".to_string();
        }

        let mut lines = vec!["Connected MCP servers:".to_string()];
        for server in servers {
            lines.push(format!("- {} ({} tool(s))", server.name, server.tools.len()));
            for tool in &server.tools {
                lines.push(format!("  - {}", tool));
            }
        }
        lines.join("\n")
    }

    fn available_for(&self, _kind: AgentKind) -> bool {
        // All agents can list MCP servers
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_mcp_tool_name() {
        let manager = Arc::new(McpManager::new(
            std::sync::Arc::new(crate::tools::registry::ToolRegistry::new())
        ));
        let tool = ConnectMcpTool::new(manager);
        assert_eq!(tool.name(), "connect_mcp");
    }

    #[test]
    fn disconnect_mcp_tool_name() {
        let manager = Arc::new(McpManager::new(
            std::sync::Arc::new(crate::tools::registry::ToolRegistry::new())
        ));
        let tool = DisconnectMcpTool::new(manager);
        assert_eq!(tool.name(), "disconnect_mcp");
    }

    #[test]
    fn list_mcp_tool_name() {
        let manager = Arc::new(McpManager::new(
            std::sync::Arc::new(crate::tools::registry::ToolRegistry::new())
        ));
        let tool = ListMcpTool::new(manager);
        assert_eq!(tool.name(), "list_mcp");
    }

    #[test]
    fn connect_mcp_lead_only() {
        let manager = Arc::new(McpManager::new(
            std::sync::Arc::new(crate::tools::registry::ToolRegistry::new())
        ));
        let tool = ConnectMcpTool::new(manager);
        assert!(tool.available_for(AgentKind::Lead));
        assert!(!tool.available_for(AgentKind::Subagent));
        assert!(!tool.available_for(AgentKind::Teammate));
    }

    #[test]
    fn disconnect_mcp_lead_only() {
        let manager = Arc::new(McpManager::new(
            std::sync::Arc::new(crate::tools::registry::ToolRegistry::new())
        ));
        let tool = DisconnectMcpTool::new(manager);
        assert!(tool.available_for(AgentKind::Lead));
        assert!(!tool.available_for(AgentKind::Subagent));
        assert!(!tool.available_for(AgentKind::Teammate));
    }

    #[test]
    fn list_mcp_all_kinds() {
        let manager = Arc::new(McpManager::new(
            std::sync::Arc::new(crate::tools::registry::ToolRegistry::new())
        ));
        let tool = ListMcpTool::new(manager);
        assert!(tool.available_for(AgentKind::Lead));
        assert!(tool.available_for(AgentKind::Subagent));
        assert!(tool.available_for(AgentKind::Teammate));
    }
}