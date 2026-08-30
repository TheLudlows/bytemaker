//! memory/retrieval.rs — MemoryRetrieval trait + model+keyword implementation.
//!
//! The `MemoryRetrieval` trait abstracts *which* memory records are relevant to a
//! query. `ModelKeywordRetrieval` is the default: ask the LLM to select indices,
//! fall back to keyword token matching on failure.
//!
//! Replace with vector ANN (embedding + HNSW) by implementing the same trait.
//!
//! See `docs/3.context_memory.md` §2.8 for the design rationale.

use async_trait::async_trait;

use crate::domain::message::{ContentBlock, Message, MessagesResponse};
use crate::providers::LlmProvider;

use super::storage::IndexEntry;

// ---- trait ----

/// Retrieval abstraction for selecting relevant memory records.
///
/// Default implementation: [`ModelKeywordRetrieval`] (model selection +
/// keyword fallback). Replaceable with vector ANN (embedding + HNSW)
/// for semantic search.
#[async_trait]
pub trait MemoryRetrieval: Send + Sync {
    /// Select relevant memory filenames from the index given a query.
    ///
    /// `client` is the LLM provider for model-based selection; `query` is the
    /// natural-language text to match against; `max_items` caps the result count.
    /// Returns filenames (keys) in relevance order.
    async fn select(
        &self,
        client: &dyn LlmProvider,
        index: &[IndexEntry],
        query: &str,
        max_items: usize,
    ) -> Vec<String>;
}

// ---- model + keyword implementation ----

/// Default implementation: model selection with keyword fallback.
///
/// Calls the LLM with a prompt listing the catalog entries + the query,
/// requesting a JSON array of indices. On any failure (network, parse, empty
/// response), degrades to keyword token matching — best-effort, never blocks
/// the agent loop.
pub struct ModelKeywordRetrieval;

impl ModelKeywordRetrieval {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MemoryRetrieval for ModelKeywordRetrieval {
    async fn select(
        &self,
        client: &dyn LlmProvider,
        index: &[IndexEntry],
        query: &str,
        max_items: usize,
    ) -> Vec<String> {
        if index.is_empty() || query.is_empty() {
            return Vec::new();
        }
        let catalog: String = index
            .iter()
            .enumerate()
            .map(|(i, e)| format!("{}: {} - {}", i, e.name, e.description))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Select memory records that are relevant to the current user request. \
             Return only a JSON array of catalog indices, such as [0, 2]. \
             Return [] when none are relevant.\n\n\
             Current request:\n{}\n\nMemory catalog:\n{}",
            take_chars(query, 4000),
            take_chars(&catalog, 12000)
        );
        let req = vec![Message::user_text(prompt)];

        match client
            .stream_messages(
                "",
                &req,
                &[],
                200,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .into_response()
        {
            Ok(response) => {
                let text = response_text(&response);
                let indices = extract_json_array(&text);
                let mut selected: Vec<String> = Vec::new();
                for idx in indices {
                    if let Some(i) = idx.as_i64() {
                        let i = i as usize;
                        if i < index.len() {
                            let filename = index[i].filename.clone();
                            if !selected.contains(&filename) {
                                selected.push(filename);
                            }
                            if selected.len() == max_items {
                                break;
                            }
                        }
                    }
                }
                tracing::info!(
                    "[memory] recall: {} records in catalog, selected {}: [{}]",
                    index.len(),
                    selected.len(),
                    selected.join(", ")
                );
                selected
            }
            Err(_) => {
                let selected = keyword_memory_selection(index, query, max_items);
                tracing::warn!(
                    "[memory] recall: LLM failed → keyword fallback, selected {}: [{}]",
                    selected.len(),
                    selected.join(", ")
                );
                selected
            }
        }
    }
}

// ---- keyword fallback (pure functions) ----

/// Keyword tokenize: ascii `[a-z0-9_]` runs >=3, or CJK (U+4E00..=U+9FFF) runs >=2.
/// Matches Python `re.findall(r"[a-z0-9_]{3,}|[一-鿿]{2,}", query.lower())`.
fn tokenize_query(query: &str) -> Vec<String> {
    let lower: String = query.to_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut kind: u8 = 0; // 0 none, 1 ascii-word, 2 cjk
    for c in lower.chars() {
        let k = if c.is_ascii_alphanumeric() || c == '_' {
            1
        } else if (c as u32) >= 0x4E00 && (c as u32) <= 0x9FFF {
            2
        } else {
            0
        };
        if k != kind {
            if !buf.is_empty() {
                let len = buf.chars().count();
                if (kind == 1 && len >= 3) || (kind == 2 && len >= 2) {
                    tokens.push(buf.clone());
                }
                buf.clear();
            }
            kind = k;
        }
        if k != 0 {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        let len = buf.chars().count();
        if (kind == 1 && len >= 3) || (kind == 2 && len >= 2) {
            tokens.push(buf);
        }
    }
    tokens
}

/// Keyword selection: rank by hit count of query tokens in name+description
/// (lowercased), take the top `max_items`.
fn keyword_memory_selection(
    index: &[IndexEntry],
    query: &str,
    max_items: usize,
) -> Vec<String> {
    let tokens = tokenize_query(query);
    let words: std::collections::HashSet<&str> = tokens.iter().map(|s| s.as_str()).collect();
    let mut ranked: Vec<(usize, String)> = Vec::new();
    for entry in index {
        let catalog_text = format!("{} {}", entry.name, entry.description).to_lowercase();
        let score = words
            .iter()
            .map(|w| if catalog_text.contains(w) { 1 } else { 0 })
            .sum::<usize>();
        if score > 0 {
            ranked.push((score, entry.filename.clone()));
        }
    }
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    ranked
        .into_iter()
        .take(max_items)
        .map(|(_, f)| f)
        .collect()
}

// ---- helpers (copy-pasted from original memory.rs; see mod.rs for the shared versions) ----

/// Take the first n chars.
fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Concatenate all Text blocks in a response into a string (for JSON parsing).
fn response_text(response: &MessagesResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the first valid JSON array in text (matching Python `raw_decode`:
/// tolerate trailing junk).
fn extract_json_array(text: &str) -> Vec<serde_json::Value> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'[' {
            continue;
        }
        let mut de =
            serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = de.next() {
            if value.is_array() {
                return value.as_array().unwrap().clone();
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_query_ascii_and_cjk() {
        let t = tokenize_query("I prefer tabs 用户偏好 ab");
        assert!(t.contains(&"prefer".to_string()));
        assert!(t.contains(&"tabs".to_string()));
        assert!(t.contains(&"用户偏好".to_string()));
        assert!(!t.contains(&"ab".to_string())); // len < 3
        assert!(!t.contains(&"i".to_string())); // len 1
    }

    #[test]
    fn keyword_selection_ranks_and_caps() {
        let index = vec![
            IndexEntry {
                filename: "a.md".into(),
                name: "prefer tabs".into(),
                description: "indentation".into(),
            },
            IndexEntry {
                filename: "b.md".into(),
                name: "database config".into(),
                description: "connection".into(),
            },
        ];
        let sel = keyword_memory_selection(&index, "tabs indentation prefer", 5);
        assert_eq!(sel, vec!["a.md".to_string()]);
        let capped = keyword_memory_selection(&index, "tabs database", 1);
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn extract_json_array_valid() {
        assert_eq!(extract_json_array("[0, 2]").len(), 2);
    }

    #[test]
    fn extract_json_array_empty_text() {
        assert!(extract_json_array("no array here").is_empty());
    }

    #[test]
    fn extract_json_array_with_leading_text() {
        let v = extract_json_array("Here are the indices:\n[1, 3]\nDone.");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].as_i64(), Some(1));
    }
}