//! memory/storage.rs — MemoryStorage trait + file-system implementation.
//!
//! The `MemoryStorage` trait abstracts where memory records are persisted.
//! `FsMemoryStorage` is the default: one `.md` file per record + a `MEMORY.md` index.
//! Replace with SlateDB or another KV store by implementing the same trait.
//!
//! See `docs/3.context_memory.md` §2.8 for the design rationale.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::AgentError;

// ---- public types ----

/// Metadata for one memory record (used in the catalog / index listing).
#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub filename: String,
    pub name: String,
    pub description: String,
}

/// A full memory document for read/write operations.
#[derive(Clone, Debug)]
pub struct MemoryDoc {
    pub name: String,
    pub mem_type: String,
    pub description: String,
    pub body: String,
}

// ---- trait ----

/// Storage abstraction for memory records.
///
/// Default implementation: [`FsMemoryStorage`] (file system).
/// Replaceable with SlateDB or other KV stores.
pub trait MemoryStorage: Send + Sync {
    /// Read a memory record by key (filename, e.g. `"user-pref.md"`).
    fn read(&self, key: &str) -> Option<String>;

    /// Write a memory record. Overwrites if `key` already exists.
    fn write(&self, key: &str, doc: &MemoryDoc) -> Result<(), AgentError>;

    /// Delete a memory record by key. No-op if `key` does not exist.
    fn delete(&self, key: &str) -> Result<(), AgentError>;

    /// List all index entries, sorted by filename.
    fn list(&self) -> Result<Vec<IndexEntry>, AgentError>;

    /// Read the master index file contents (empty string if missing).
    fn read_index(&self) -> String;

    /// Write the master index file from a list of entries.
    fn write_index(&self, entries: &[IndexEntry]) -> Result<(), AgentError>;
}

// ---- file-system implementation ----

const MEMORY_INDEX_FILENAME: &str = "MEMORY.md";

/// File-system implementation of [`MemoryStorage`].
///
/// Each record is a `.md` file with YAML frontmatter; the index is `MEMORY.md`.
/// Records are stored as:
/// ```text
/// ---
/// name: <name>
/// description: <description>
/// type: <mem_type>
/// ---
///
/// <body>
/// ```
pub struct FsMemoryStorage {
    memory_dir: PathBuf,
}

impl FsMemoryStorage {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }

    /// Return the memory directory path.
    pub fn dir(&self) -> &Path {
        &self.memory_dir
    }

    /// Validate a filename key: no path separators, no `..`/`.`,
    /// not the index file itself.
    fn validate_key(key: &str) -> Result<(), String> {
        if Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            != Some(key)
        {
            return Err(format!("Invalid memory filename: {}", key));
        }
        if key == MEMORY_INDEX_FILENAME {
            return Err("The memory index is not a memory record".to_string());
        }
        Ok(())
    }

    /// Ensure the memory directory exists.
    fn ensure_dir(&self) -> Result<(), AgentError> {
        fs::create_dir_all(&self.memory_dir)?;
        Ok(())
    }
}

impl MemoryStorage for FsMemoryStorage {
    fn read(&self, key: &str) -> Option<String> {
        FsMemoryStorage::validate_key(key).ok()?;
        let path = self.memory_dir.join(key);
        if path.is_file() {
            fs::read_to_string(&path).ok()
        } else {
            None
        }
    }

    fn write(&self, key: &str, doc: &MemoryDoc) -> Result<(), AgentError> {
        FsMemoryStorage::validate_key(key).map_err(AgentError::Other)?;
        self.ensure_dir()?;
        let path = self.memory_dir.join(key);
        let content = memory_document(&doc.name, &doc.mem_type, &doc.description, &doc.body);
        fs::write(&path, content)?;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), AgentError> {
        FsMemoryStorage::validate_key(key).map_err(AgentError::Other)?;
        let path = self.memory_dir.join(key);
        if path.is_file() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<IndexEntry>, AgentError> {
        if !self.memory_dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries: Vec<IndexEntry> = Vec::new();
        for entry in fs::read_dir(&self.memory_dir)? {
            let entry = entry?;
            let path = entry.path();
            let fname = match path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if fname == MEMORY_INDEX_FILENAME || !fname.ends_with(".md") || !path.is_file() {
                continue;
            }
            if FsMemoryStorage::validate_key(&fname).is_err() {
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let (fm, body) = parse_frontmatter(&content);
            let name = fm
                .name
                .as_deref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(normalize_ws)
                .unwrap_or_else(|| {
                    path.file_stem()
                        .map(|s| normalize_ws(&s.to_string_lossy()))
                        .unwrap_or_default()
                });
            let first_line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let description = fm
                .description
                .as_deref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(normalize_ws)
                .unwrap_or_else(|| normalize_ws(first_line));
            entries.push(IndexEntry {
                filename: fname,
                name,
                description,
            });
        }
        entries.sort_by(|a, b| a.filename.cmp(&b.filename));
        Ok(entries)
    }

    fn read_index(&self) -> String {
        let path = self.memory_dir.join(MEMORY_INDEX_FILENAME);
        if path.exists() {
            fs::read_to_string(&path)
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        }
    }

    fn write_index(&self, entries: &[IndexEntry]) -> Result<(), AgentError> {
        self.ensure_dir()?;
        let content = if entries.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = entries
                .iter()
                .map(|e| format!("- [{}]({}) - {}", e.name, e.filename, e.description))
                .collect();
            format!("{}\n", lines.join("\n"))
        };
        let path = self.memory_dir.join(MEMORY_INDEX_FILENAME);
        fs::write(&path, content)?;
        Ok(())
    }
}

// ---- markdown formatting helpers (private) ----

/// Frontmatter fields we care about (others ignored); `type` is a Rust keyword, so rename it.
#[derive(Default, Deserialize, Clone, Debug)]
pub(crate) struct MemoryFrontmatter {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    #[serde(rename = "type")]
    pub(crate) mem_type: Option<String>,
}

/// Frontmatter serialized on write (field order = name/description/type, unsorted).
#[derive(Serialize)]
struct FrontmatterOut<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(rename = "type")]
    mem_type: &'a str,
}

/// Parse YAML frontmatter with a tolerant fallback (tolerates a BOM,
/// same pattern as `skills.rs`). Returns (frontmatter, body).
pub(crate) fn parse_frontmatter_crate(text: &str) -> (MemoryFrontmatter, String) {
    parse_frontmatter(text)
}

/// Parse YAML frontmatter with a tolerant fallback (tolerates a BOM,
/// same pattern as `skills.rs`). Returns (frontmatter, body).
fn parse_frontmatter(text: &str) -> (MemoryFrontmatter, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if !text.starts_with("---") {
        return (MemoryFrontmatter::default(), text.to_string());
    }
    let parts: Vec<&str> = text.splitn(3, "---").collect();
    if parts.len() < 3 {
        return (MemoryFrontmatter::default(), text.to_string());
    }
    let fm_text = parts[1];
    let body = parts[2].trim_start_matches(['\r', '\n']).to_string();
    match serde_yaml::from_str::<MemoryFrontmatter>(fm_text) {
        Ok(fm) => (fm, body),
        Err(_) => (MemoryFrontmatter::default(), text.to_string()),
    }
}

/// Memory document written to disk: `---\n{frontmatter}\n---\n\n{body}\n`.
fn memory_document(name: &str, mem_type: &str, description: &str, body: &str) -> String {
    let fm = FrontmatterOut {
        name,
        description,
        mem_type,
    };
    let fm_str = serde_yaml::to_string(&fm).unwrap_or_default();
    format!("---\n{}\n---\n\n{}\n", fm_str.trim(), body.trim())
}

/// Normalize whitespace: split_whitespace then join with single spaces
/// (matching Python `" ".join(s.split())`).
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage(label: &str) -> (FsMemoryStorage, PathBuf) {
        let dir = std::env::temp_dir().join(format!("bytemaker-memstorage-{}", label));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (FsMemoryStorage::new(dir.clone()), dir)
    }

    // ---- validate_key ----

    #[test]
    fn validate_key_rejects_separators_and_dotdot() {
        let (s, _dir) = temp_storage("key-reject");
        assert!(FsMemoryStorage::validate_key("a/b.md").is_err());
        assert!(FsMemoryStorage::validate_key("..").is_err());
        assert!(FsMemoryStorage::validate_key(".").is_err());
        assert!(FsMemoryStorage::validate_key("good.md").is_ok());
        let _ = fs::remove_dir_all(_dir);
    }

    #[test]
    fn validate_key_rejects_index() {
        assert!(FsMemoryStorage::validate_key("MEMORY.md").is_err());
    }

    // ---- write / read round-trip ----

    #[test]
    fn write_then_read() {
        let (s, _dir) = temp_storage("write-read");
        let doc = MemoryDoc {
            name: "User preference tabs".into(),
            mem_type: "user".into(),
            description: "prefer tabs".into(),
            body: "Use tabs for indent.".into(),
        };
        s.write("user-preference-tabs.md", &doc).unwrap();
        let content = s.read("user-preference-tabs.md").unwrap();
        assert!(content.contains("name: User preference tabs"));
        assert!(content.contains("type: user"));
        assert!(content.contains("Use tabs for indent."));
        let _ = fs::remove_dir_all(_dir);
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let (s, _dir) = temp_storage("read-none");
        assert!(s.read("nonexistent.md").is_none());
        let _ = fs::remove_dir_all(_dir);
    }

    #[test]
    fn delete_removes_file() {
        let (s, _dir) = temp_storage("delete");
        let doc = MemoryDoc {
            name: "t".into(),
            mem_type: "user".into(),
            description: "d".into(),
            body: "b".into(),
        };
        s.write("t.md", &doc).unwrap();
        assert!(s.read("t.md").is_some());
        s.delete("t.md").unwrap();
        assert!(s.read("t.md").is_none());
        let _ = fs::remove_dir_all(_dir);
    }

    #[test]
    fn delete_nonexistent_is_noop() {
        let (s, _dir) = temp_storage("delete-noop");
        assert!(s.delete("nonexistent.md").is_ok());
        let _ = fs::remove_dir_all(_dir);
    }

    // ---- list ----

    #[test]
    fn list_sorted_by_filename() {
        let (s, _dir) = temp_storage("list");
        s.write(
            "beta.md",
            &MemoryDoc {
                name: "Beta".into(),
                mem_type: "project".into(),
                description: "b desc".into(),
                body: "b body".into(),
            },
        )
        .unwrap();
        s.write(
            "alpha.md",
            &MemoryDoc {
                name: "Alpha".into(),
                mem_type: "user".into(),
                description: "a desc".into(),
                body: "a body".into(),
            },
        )
        .unwrap();
        let entries = s.list().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].filename, "alpha.md");
        assert_eq!(entries[0].name, "Alpha");
        assert_eq!(entries[1].filename, "beta.md");
        assert_eq!(entries[1].name, "Beta");
        // index file is excluded
        assert!(entries.iter().all(|e| e.filename != "MEMORY.md"));
        let _ = fs::remove_dir_all(_dir);
    }

    #[test]
    fn list_empty_dir() {
        let (s, _dir) = temp_storage("list-empty");
        let entries = s.list().unwrap();
        assert!(entries.is_empty());
        let _ = fs::remove_dir_all(_dir);
    }

    // ---- index ----

    #[test]
    fn write_index_then_read_index() {
        let (s, _dir) = temp_storage("index");
        let entries = vec![
            IndexEntry {
                filename: "a.md".into(),
                name: "A".into(),
                description: "desc a".into(),
            },
            IndexEntry {
                filename: "b.md".into(),
                name: "B".into(),
                description: "desc b".into(),
            },
        ];
        s.write_index(&entries).unwrap();
        let index = s.read_index();
        assert!(index.contains("- [A](a.md) - desc a"));
        assert!(index.contains("- [B](b.md) - desc b"));
        let _ = fs::remove_dir_all(_dir);
    }

    #[test]
    fn read_index_empty_when_missing() {
        let (s, _dir) = temp_storage("index-missing");
        assert_eq!(s.read_index(), "");
        let _ = fs::remove_dir_all(_dir);
    }

    // ---- frontmatter parse / document ----

    #[test]
    fn parse_frontmatter_normal() {
        let text = "---\nname: tabs\ndescription: prefer tabs\ntype: user\n---\n\nbody here";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("tabs"));
        assert_eq!(fm.description.as_deref(), Some("prefer tabs"));
        assert_eq!(fm.mem_type.as_deref(), Some("user"));
        assert_eq!(body, "body here");
    }

    #[test]
    fn parse_frontmatter_missing_falls_back_to_full_text() {
        let text = "# just a heading\nno fm";
        let (fm, body) = parse_frontmatter(text);
        assert!(fm.name.is_none());
        assert_eq!(body, text);
    }

    #[test]
    fn parse_frontmatter_malformed_yaml_falls_back() {
        let text = "---\nname: : :\n---\nbody";
        let (fm, body) = parse_frontmatter(text);
        assert!(fm.name.is_none());
        assert!(body.starts_with("---"));
    }

    #[test]
    fn memory_document_round_trips() {
        let doc = memory_document("Tabs", "user", "prefer tabs", "Use tabs for indent.");
        let (fm, body) = parse_frontmatter(&doc);
        assert_eq!(fm.name.as_deref(), Some("Tabs"));
        assert_eq!(fm.mem_type.as_deref(), Some("user"));
        assert_eq!(body.trim(), "Use tabs for indent.");
    }

    #[test]
    fn list_falls_back_to_stem_and_first_line() {
        let (s, dir) = temp_storage("list-fallback");
        // Write a .md with no frontmatter directly; list should fall back to stem + first body line.
        fs::write(dir.join("plain.md"), "# Heading\nfirst body line\nsecond").unwrap();
        let entries = s.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "plain.md");
        assert_eq!(entries[0].name, "plain");
        assert_eq!(entries[0].description, "# Heading");
        let _ = fs::remove_dir_all(dir);
    }
}