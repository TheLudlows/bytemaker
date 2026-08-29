//! Stable identity helpers: content-hash agent keys and run ids.
//!
//! `stable_hash` uses sha256 (process-stable, unlike std `hash()` which is
//! salted per process — that would break resume keys across `run`/`resume`).

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Process-stable content hash.
pub fn stable_hash(s: &str) -> u64 {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let bytes = h.finalize();
    u64::from_be_bytes(bytes[..8].try_into().unwrap())
}

/// Deterministic agent-call key, independent of concurrency order.
pub fn agent_key(label: &str, prompt: &str, schema: Option<&Value>) -> String {
    let schema_str = schema
        .map(|s| serde_json::to_string(s).unwrap_or_default())
        .unwrap_or_default();
    let basis = format!("agent|{label}|{prompt}|{schema_str}");
    format!("agent-{:010}", stable_hash(&basis) % 10_000_000_000)
}

pub fn create_run_id(name: &str) -> String {
    let n = fastrand::u64(..);
    format!("wf_{name}_{n:016x}")
}

/// `meta.name` must be a 1-64 char slug: alnum + `.`/`_`/`-`, leading alnum.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    let rest: String = chars.collect();
    rest.len() <= 63
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

pub fn validate_run_id(run_id: &str) -> bool {
    let s = match run_id.strip_prefix("wf_") {
        Some(s) => s,
        None => return false,
    };
    let (name, hex) = match s.rsplit_once('_') {
        Some(t) => t,
        None => return false,
    };
    is_valid_name(name) && hex.len() == 16 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stable_hash_deterministic() {
        assert_eq!(stable_hash("abc"), stable_hash("abc"));
        assert_ne!(stable_hash("abc"), stable_hash("abd"));
    }

    #[test]
    fn agent_key_order_independent() {
        let k1 = agent_key("audit:security", "prompt A", Some(&json!({"x":1})));
        let k2 = agent_key("audit:security", "prompt A", Some(&json!({"x":1})));
        assert_eq!(k1, k2);
    }

    #[test]
    fn agent_key_differs_on_prompt() {
        assert_ne!(agent_key("l", "p1", None), agent_key("l", "p2", None));
    }

    #[test]
    fn name_validation() {
        assert!(is_valid_name("review-changes"));
        assert!(is_valid_name("a.b_c-1"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-bad")); // leading dash
        assert!(!is_valid_name(&"x".repeat(65)));
    }

    #[test]
    fn run_id_roundtrip() {
        let id = create_run_id("review-changes");
        assert!(validate_run_id(&id));
        assert!(!validate_run_id("bogus"));
    }
}
