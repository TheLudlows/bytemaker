//! Host registry of trusted workflows.
//!
//! Workflows are trusted Rust functions registered here — the model only
//! submits a name + args, never executable code (same trust model as s16).
//! The registry auto-initializes with built-in workflows on first access.

use crate::workflow::state::ExecutionState;
use crate::workflow::WorkflowError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phases: Option<Vec<String>>,
}

#[async_trait]
pub trait Workflow: Send + Sync {
    async fn run(&self, ctx: &ExecutionState<'_>, args: &Value) -> Result<Value, WorkflowError>;
}

static REGISTRY: OnceLock<BTreeMap<String, (Meta, Arc<dyn Workflow>)>> = OnceLock::new();

/// Access the trusted host registry. Auto-initializes with built-in workflows
/// (currently the `review-changes` sample) on first call.
pub fn workflows() -> &'static BTreeMap<String, (Meta, Arc<dyn Workflow>)> {
    REGISTRY.get_or_init(|| {
        let mut m: BTreeMap<String, (Meta, Arc<dyn Workflow>)> = BTreeMap::new();
        let (meta, wf) = crate::workflow::sample::sample_entry();
        m.insert(meta.name.clone(), (meta, wf));
        m
    })
}
