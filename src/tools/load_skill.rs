/*
load_skill.rs - Load Skill Tool

- Tool trait impl for loading skill definitions
- Reads ctx.agent.skills (owned by Agent); default Pass
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Load Skill Tool — load a skill's SKILL.md by name.
pub struct LoadSkillTool;

#[async_trait]
impl Tool for LoadSkillTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "load_skill"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Load a skill definition by name. Returns the complete SKILL.md content."
    }

    /// Returns the JSON schema for load_skill input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the skill to load"
                }
            },
            "required": ["name"]
        })
    }

    /// Permission check for load_skill tool (default Pass)
    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    /// Executes the skill loading operation
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        match input.get("name").and_then(|n| n.as_str()) {
            Some(name) => ctx.agent.skills.load(name),
            None => "Error: missing name".to_string(),
        }
    }
}