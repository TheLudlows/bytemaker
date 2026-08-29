/*
mcp/tool.rs - McpTool: Adapter for MCP tools to Tool trait

This module implements McpTool which adapts MCP server tools to bytemaker's
Tool trait, allowing external MCP tools to be dispatched through the same
pipeline as built-in tools.
*/

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::Value;
use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
use crate::error::AgentError;
use super::client::McpClient;

/// Trait for MCP client operations (allows mocking)
#[async_trait]
pub trait McpClientTrait: Send + Sync {
    async fn call_tool(&self, name: &str, args: &Value) -> Result<String, AgentError>;
}

/// Implement McpClientTrait for the real McpClient
#[async_trait]
impl McpClientTrait for McpClient {
    async fn call_tool(&self, name: &str, args: &Value) -> Result<String, AgentError> {
        McpClient::call_tool(self, name, args).await
    }
}

/// Mock MCP client for testing
pub struct MockMcpClient;

impl MockMcpClient {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpClientTrait for MockMcpClient {
    async fn call_tool(&self, _name: &str, _args: &Value) -> Result<String, AgentError> {
        Ok("mock result".to_string())
    }
}

/// Adapter that wraps an MCP tool and implements the Tool trait.
pub struct McpTool {
    /// Normalized prefixed tool name (mcp__{server}__{tool})
    prefixed_name: String,
    /// Server's original tool name (for calling back to MCP server)
    raw_name: String,
    /// Server name (for identification)
    #[allow(dead_code)]
    server_name: String,
    /// Tool description
    description: String,
    /// Input JSON Schema
    input_schema: Value,
    /// MCP client reference (for tools/call)
    client: Arc<dyn McpClientTrait>,
}

impl McpTool {
    pub fn new(
        prefixed_name: String,
        raw_name: String,
        server_name: String,
        description: String,
        input_schema: Value,
        client: Arc<dyn McpClientTrait>,
    ) -> Self {
        Self {
            prefixed_name,
            raw_name,
            server_name,
            description,
            input_schema,
            client,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        // v1: All MCP tools require user confirmation
        PermissionCheck::NeedsApproval("External MCP tool call requires confirmation")
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        match self.client.call_tool(&self.raw_name, input).await {
            Ok(result) => result,
            Err(e) => format!("MCP error: {}", e),
        }
    }

    fn available_for(&self, _kind: AgentKind) -> bool {
        // MCP tools visible to Lead / Subagent / Teammate
        true
    }
}

#[cfg(test)]
mod mcp_tool_tests {
    use super::*;

    #[test]
    fn mcp_tool_name_returns_prefixed_name() {
        let tool = McpTool::new(
            "mcp__test__tool".to_string(),
            "tool".to_string(),
            "test".to_string(),
            "test tool".to_string(),
            serde_json::json!({}),
            Arc::new(MockMcpClient::new()),
        );
        assert_eq!(tool.name(), "mcp__test__tool");
    }

    #[test]
    fn mcp_tool_requires_approval() {
        let tool = McpTool::new(
            "mcp__test__tool".to_string(),
            "tool".to_string(),
            "test".to_string(),
            "test tool".to_string(),
            serde_json::json!({}),
            Arc::new(MockMcpClient::new()),
        );
        let check = tool.check_permission(&serde_json::json!({}));
        assert!(matches!(check, PermissionCheck::NeedsApproval(_)));
    }

    #[test]
    fn mcp_tool_available_for_all_kinds() {
        let tool = McpTool::new(
            "mcp__test__tool".to_string(),
            "tool".to_string(),
            "test".to_string(),
            "test tool".to_string(),
            serde_json::json!({}),
            Arc::new(MockMcpClient::new()),
        );
        assert!(tool.available_for(AgentKind::Lead));
        assert!(tool.available_for(AgentKind::Subagent));
        assert!(tool.available_for(AgentKind::Teammate));
    }
}
