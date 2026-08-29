//! Sample workflow: review-changes (port of s16 sample_workflow).
//!
//! pipeline over review dimensions (audit -> verify-each), then keep only the
//! findings a verifier confirms. The plan is code, not a chat turn.

use crate::workflow::registry::{Meta, Workflow};
use crate::workflow::state::{BoxFut, ExecutionState, PipelineStage};
use crate::workflow::WorkflowError;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::sync::Arc;

pub static FINDINGS_SCHEMA: OnceLock<Value> = OnceLock::new();
pub static VERDICT_SCHEMA: OnceLock<Value> = OnceLock::new();

pub fn findings_schema() -> &'static Value {
    FINDINGS_SCHEMA.get_or_init(|| {
        json!({
            "type": "object", "required": ["findings"],
            "properties": { "findings": { "type": "array", "items": {
                "type": "object", "required": ["title", "severity"],
                "properties": {
                    "title": { "type": "string" },
                    "severity": { "type": "string", "enum": ["high", "medium", "low"] }
                }
            }}}
        })
    })
}

pub fn verdict_schema() -> &'static Value {
    VERDICT_SCHEMA.get_or_init(|| {
        json!({
            "type": "object", "required": ["isReal", "reason"],
            "properties": {
                "isReal": { "type": "boolean" },
                "reason": { "type": "string" }
            }
        })
    })
}

pub const DEMO_CHANGES: &str =
    "def load_user(user_id):\n    query = f\"SELECT * FROM users WHERE id = {user_id}\"\n    return db.execute(query).fetchone()\n";

const DIMENSIONS: &[&str] = &["correctness", "security", "performance", "style"];

pub fn sample_meta() -> Meta {
    Meta {
        name: "review-changes".into(),
        description: "Review changed files across dimensions, verify each finding".into(),
        phases: Some(vec!["Review".into(), "Verify".into()]),
    }
}

pub fn sample_entry() -> (Meta, Arc<dyn Workflow>) {
    (sample_meta(), Arc::new(ReviewChangesWorkflow))
}

/// Ensure the host registry is initialized (idempotent).
pub fn register_sample() {
    let _ = crate::workflow::registry::workflows();
}

pub struct ReviewChangesWorkflow;

#[async_trait]
impl Workflow for ReviewChangesWorkflow {
    async fn run(&self, ctx: &ExecutionState<'_>, args: &Value) -> Result<Value, WorkflowError> {
        ctx.phase("Review");
        let changes = args.get("changes").and_then(Value::as_str).unwrap_or("");
        let review_input = if changes.trim().is_empty() {
            "No change context was supplied."
        } else {
            changes
        };

        let audit = AuditStage { review_input: review_input.to_string() };
        let verify = VerifyStage { review_input: review_input.to_string() };

        let items: Vec<Value> = DIMENSIONS.iter().map(|d| json!({"dimension": d})).collect();
        let stages: Vec<Arc<dyn PipelineStage>> = vec![Arc::new(audit), Arc::new(verify)];
        let results = ctx.pipeline(items, stages).await?;

        let mut confirmed: Vec<Value> = Vec::new();
        for r in results.iter().filter(|r| r.is_object()) {
            let dim = r.get("dimension").cloned().unwrap_or(Value::Null);
            if let Some(arr) = r.get("confirmed").and_then(Value::as_array) {
                for f in arr {
                    let mut entry = serde_json::Map::new();
                    entry.insert("dimension".into(), dim.clone());
                    if let Value::Object(m) = f {
                        entry.extend(m.clone());
                    }
                    confirmed.push(Value::Object(entry));
                }
            }
        }
        let rank = |v: &Value| -> u8 {
            match v.get("severity").and_then(Value::as_str) {
                Some("high") => 0,
                Some("medium") => 1,
                Some("low") => 2,
                _ => 3,
            }
        };
        confirmed.sort_by_key(rank);
        ctx.log(&format!("confirmed {} real finding(s)", confirmed.len()));
        Ok(json!({"confirmed": confirmed}))
    }
}

struct AuditStage {
    review_input: String,
}

#[async_trait]
impl PipelineStage for AuditStage {
    async fn run(
        &self,
        ctx: &ExecutionState<'_>,
        prev: Value,
        _item: Value,
        _idx: usize,
    ) -> Result<Value, WorkflowError> {
        let dimension = prev.get("dimension").and_then(Value::as_str).unwrap_or("dimension");
        let out = ctx
            .agent(
                &format!(
                    "Review this change context for {dimension} issues. Report only issues \
                     supported by the supplied text.\n\n{}",
                    self.review_input
                ),
                Some(findings_schema()),
                Some(&format!("audit:{dimension}")),
                Some("Review"),
            )
            .await?;
        Ok(json!({"dimension": dimension, "findings": out["findings"].clone()}))
    }
}

struct VerifyStage {
    review_input: String,
}

#[async_trait]
impl PipelineStage for VerifyStage {
    async fn run(
        &self,
        ctx: &ExecutionState<'_>,
        prev: Value,
        _item: Value,
        _idx: usize,
    ) -> Result<Value, WorkflowError> {
        ctx.phase("Verify");
        let dimension = prev.get("dimension").and_then(Value::as_str).unwrap_or("dimension");
        let findings = prev
            .get("findings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let thunks: Vec<BoxFut<'_>> = findings
            .iter()
            .map(|f| {
                let f = f.clone();
                let ri = self.review_input.clone();
                let dim = dimension.to_string();
                Box::pin(async move {
                    ctx.agent(
                        &format!(
                            "Adversarially verify this {dim} finding against the supplied \
                             change context.\n\nChange context:\n{ri}\n\nFinding:\n{}",
                            serde_json::to_string(&f).unwrap_or_default()
                        ),
                        Some(verdict_schema()),
                        Some(&format!(
                            "verify:{dim}:{}",
                            f.get("title").and_then(Value::as_str).unwrap_or("?")
                        )),
                        Some("Verify"),
                    )
                    .await
                }) as BoxFut<'_>
            })
            .collect();
        let verdicts = ctx.parallel(thunks).await?;

        let confirmed: Vec<Value> = findings
            .iter()
            .zip(verdicts.iter())
            .filter(|(_, v)| v.get("isReal").and_then(Value::as_bool).unwrap_or(false))
            .map(|(f, _)| f.clone())
            .collect();
        Ok(json!({"dimension": dimension, "confirmed": confirmed}))
    }
}
