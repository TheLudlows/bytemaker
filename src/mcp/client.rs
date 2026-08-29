/*
mcp/client.rs - Simplified MCP client using rmcp

This module provides a simplified MCP client wrapper around rmcp's
TokioChildProcess transport and Peer API for stdio communication.
*/

use std::sync::Arc;
use serde_json::Value;
use crate::error::AgentError;

/// Re-export rmcp types for convenience
pub use rmcp::{
    model::*,
    service::{serve_client, Peer, RunningService},
    transport::TokioChildProcess,
};

/// Initialize result from MCP server
#[derive(Debug, Clone)]
pub struct InitResult {
    pub capabilities: ServerCapabilities,
}

/// MCP tool definition from server
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Simplified MCP client wrapper using rmcp
pub struct McpClient {
    /// The rmcp peer for communicating with the server
    peer: Peer<rmcp::RoleClient>,
    /// The running service (kept for cleanup)
    _service: Arc<RunningService<rmcp::RoleClient, ClientInfo>>,
}

impl McpClient {
    /// Spawn a child process and establish MCP connection using rmcp
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, AgentError> {
        use tokio::process::Command;

        let mut cmd = Command::new(command);
        cmd.args(args);

        // Create rmcp transport from child process (not async)
        let transport = TokioChildProcess::new(cmd)
            .map_err(|e| AgentError::Other(format!("Failed to create MCP transport: {}", e)))?;

        // Create client info for initialization
        let client_info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("bytemaker", "1.0.0")
        );

        // Serve the client with rmcp - returns RunningService
        let running_service = serve_client(client_info, transport)
            .await
            .map_err(|e| AgentError::Other(format!("Failed to serve MCP client: {}", e)))?;

        // Get the peer from the running service
        let peer = running_service.peer().clone();

        Ok(Self {
            peer,
            _service: Arc::new(running_service),
        })
    }

    /// Initialize the connection (rmcp handles this during spawn)
    pub async fn initialize(&self) -> Result<InitResult, AgentError> {
        // Get peer info which contains server capabilities after initialization
        let peer_info = self.peer.peer_info()
            .ok_or_else(|| AgentError::Other("Peer not initialized".into()))?;

        Ok(InitResult {
            capabilities: peer_info.capabilities.clone(),
        })
    }

    /// List available tools from MCP server
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, AgentError> {
        let result = self.peer.list_tools(None).await
            .map_err(|e| AgentError::Other(format!("Failed to list tools: {}", e)))?;

        Ok(result.tools.into_iter().map(|tool| McpToolDef {
            name: tool.name.to_string(),
            description: tool.description.as_ref().map(|d| d.to_string()).unwrap_or_default(),
            input_schema: tool.schema_as_json_value(),
        }).collect())
    }

    /// Call a tool on the MCP server
    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<String, AgentError> {
        let params = CallToolRequestParams::new(name.to_string());

        // Convert Value to JsonObject if needed
        let arguments = match args {
            Value::Object(map) => map.clone(),
            _ => return Err(AgentError::Other("Tool arguments must be a JSON object".into())),
        };

        let params = params.with_arguments(arguments);

        let result = self.peer.call_tool(params).await
            .map_err(|e| AgentError::Other(format!("Failed to call tool: {}", e)))?;

        // Extract text content from the result
        let texts: Vec<String> = result.content.clone().into_iter().filter_map(|content| {
            match content {
                ContentBlock::Text(text_content) => Some(text_content.text),
                ContentBlock::Image(_) | ContentBlock::Audio(_) |
                ContentBlock::Resource(_) | ContentBlock::ResourceLink(_) => None,
                _ => None,
            }
        }).collect();

        if texts.is_empty() {
            Ok(format!("{:?}", result))
        } else {
            Ok(texts.join("\n"))
        }
    }

    /// Shutdown the client (rmcp handles cleanup when RunningService is dropped)
    pub async fn shutdown(self) -> Result<(), AgentError> {
        // rmcp's RunningService handles cleanup on drop
        Ok(())
    }
}

// Allow cloning for use with Arc<McpClient> pattern
impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            peer: self.peer.clone(),
            _service: Arc::clone(&self._service),
        }
    }
}

// Implement Display for the tracing macro in mod.rs
impl std::fmt::Display for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "McpClient")
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;

    #[tokio::test]
    async fn tool_def_creation() {
        let tool = McpToolDef {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };

        assert_eq!(tool.name, "test_tool");
        assert_eq!(tool.description, "A test tool");
        assert!(tool.input_schema.is_object());
    }

    #[test]
    fn init_result_creation() {
        let result = InitResult {
            capabilities: ServerCapabilities::default(),
        };

        // Just verify we can create it
        drop(result);
    }

    #[tokio::test]
    async fn integration_test_mcp_protocol_interactions() {
        // This test exercises actual MCP protocol interactions with a simple mock server
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a simple Python script that acts as a mock MCP server
        let mock_server_script = r#"
import sys
import json

def send_response(response):
    print(json.dumps(response))
    sys.stdout.flush()

while True:
    try:
        line = sys.stdin.readline()
        if not line:
            break

        request = json.loads(line.strip())
        request_id = request.get("id", 0)
        method = request.get("method", "")

        if method == "initialize":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "mock-server",
                        "version": "1.0.0"
                    }
                }
            })
        elif method == "notifications/initialized":
            # Just acknowledge, no response needed
            pass
        elif method == "tools/list":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "tools": [
                        {
                            "name": "test_tool",
                            "description": "A test tool",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "param1": {"type": "string"}
                                }
                            }
                        }
                    ]
                }
            })
        elif method == "tools/call":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "content": [
                        {
                            "type": "text",
                            "text": "Tool execution result"
                        }
                    ]
                }
            })
        elif method == "shutdown":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {}
            })
            break
        else:
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32601,
                    "message": "Method not found"
                }
            })
    except Exception as e:
        send_response({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {
                "code": -32700,
                "message": str(e)
            }
        })
        break
"#;

        // Write the mock server script to a temporary file
        let mut temp_file = NamedTempFile::new()
            .expect("Failed to create temp file");
        temp_file.write_all(mock_server_script.as_bytes())
            .expect("Failed to write mock server script");
        let temp_path = temp_file.path().to_str().expect("Invalid path");

        // Test 1: Spawn and initialize client
        let client = McpClient::spawn("python", &[temp_path])
            .await
            .expect("Failed to spawn MCP client");

        let init_result = client.initialize()
            .await
            .expect("Failed to initialize MCP client");

        // Just verify we got capabilities
        drop(init_result);

        // Test 2: List tools
        let tools = client.list_tools()
            .await
            .expect("Failed to list tools");

        assert_eq!(tools.len(), 1, "Should have exactly one tool");
        assert_eq!(tools[0].name, "test_tool", "Tool name should be 'test_tool'");
        assert_eq!(tools[0].description, "A test tool", "Tool description should match");

        // Test 3: Call tool
        let tool_args = serde_json::json!({"param1": "test_value"});
        let result = client.call_tool("test_tool", &tool_args)
            .await
            .expect("Failed to call tool");

        assert!(result.contains("Tool execution result"), "Tool result should contain expected text");

        // Test 4: Shutdown client (this tests the cleanup logic)
        client.shutdown()
            .await
            .expect("Failed to shutdown client");

        // Clean up temp file
        let _ = std::fs::remove_file(temp_path);
    }
}