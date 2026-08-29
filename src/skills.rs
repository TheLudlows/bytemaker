//! skills.rs — skill loading (s07).
//!
//! Scans `skills/<name>/SKILL.md` at startup, parses YAML frontmatter for name/description,
//! and catalogs them into the system prompt. `load_skill(name)` returns the full `SKILL.md`
//! body as a `tool_output`.
//!
//! ```text
//!     skills/                    at startup
//!     +------------------+       +------------------+
//!     | code-review/     | ----> | SkillLoader      |
//!     |   SKILL.md       |       | name + summary   |
//!     | pdf/             |       +--------+---------+
//!     |   SKILL.md       |                |
//!     +------------------+                v
//!                                system prompt catalog
//!
//!     LLM -- load_skill(name) --> full SKILL.md
//!      ^                              |
//!      +--------- tool_output --------+
//! ```
//!
//! | Content | Enters model at | When |
//! |---|---|---|
//! | skill name + description | system prompt | at startup |
//! | full `SKILL.md` | `tool_output` | on `load_skill` call |
//!
//! Held read-only by `Agent` (`Arc<SkillLoader>`): no writes after scan, no `Mutex` needed;
//! passed to the `load_skill` tool via `ToolContext.agent.skills`.
//!
//! Python reference: `s07_skill_loading/code.py` `SkillLoader`.
//!
//! Details: `docs/modules/skills.md`.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// A skill: name, description, full `SKILL.md` body.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Full `SKILL.md` text (incl. frontmatter), returned as `tool_output`.
    pub content: String,
}

/// YAML frontmatter fields we care about (others ignored). Both optional, fall back if absent.
#[derive(Default, Deserialize, Clone, Debug)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Skill loader: scans once at startup, then read-only.
pub struct SkillLoader {
    /// Registry sorted by name: deterministic order, O(log n) lookup; later same-name overwrites earlier (like Python dict).
    skills: BTreeMap<String, Skill>,
}

impl SkillLoader {
    /// Scan `skills_dir/*/SKILL.md` and build the registry.
    ///
    /// Returns empty registry if `skills_dir` is missing/unreadable (no panic, no error — agent runs without skills).
    /// Scans only direct subdirectories (Python `glob("*/SKILL.md")`); skill `references/`/`scripts/` aren't treated as skills.
    pub fn scan(skills_dir: PathBuf) -> SkillLoader {
        let mut skills: BTreeMap<String, Skill> = BTreeMap::new();

        if let Ok(entries) = fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let manifest = path.join("SKILL.md");
                let content = match fs::read_to_string(&manifest) {
                    Ok(c) => c,
                    Err(_) => continue, // no SKILL.md in subdir, skip
                };

                let dir_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());

                let (fm, body) = parse_frontmatter(&content);

                let name = fm
                    .name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(dir_name);

                // description: frontmatter first, else fall back to first body line (normalized after stripping # / leading whitespace).
                let description = fm
                    .description
                    .map(|d| normalize_description(&d))
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| {
                        body.lines()
                            .find(|l| !l.trim().is_empty())
                            .map(normalize_description)
                            .unwrap_or_default()
                    });

                skills.insert(
                    name.clone(),
                    Skill {
                        name,
                        description,
                        content,
                    },
                );
            }
        }

        SkillLoader { skills }
    }

    /// Number of skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Check if there are any skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Skill catalog (name + description) for the system prompt.
    /// One line per skill: `- {name}: {description}`; empty string if no skills.
    pub fn catalog(&self) -> String {
        self.skills
            .values()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Look up by name (not file path), return full `SKILL.md` body.
    /// On miss, return an error string listing available skills. `dispatch_tool` wraps it as `[ERROR:load_skill] ...` via its `Error:` prefix logic.
    pub fn load(&self, name: &str) -> String {
        match self.skills.get(name) {
            Some(skill) => skill.content.clone(),
            None => {
                let available = self
                    .skills
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let available = if available.is_empty() {
                    "none".to_string()
                } else {
                    available
                };
                format!("Error: Unknown skill '{}'. Available: {}", name, available)
            }
        }
    }
}

/// Parse YAML frontmatter.
///
/// If the file starts with `---`, parse between the `---` delimiters via `serde_yaml`; the rest is the body.
/// Falls back to `{ name: None, description: None }` + full text as body on missing frontmatter,
/// too few segments, YAML parse failure, or non-mapping result — never panics.
/// Matches the Python `parse_frontmatter` fallback behavior.
fn parse_frontmatter(text: &str) -> (SkillFrontmatter, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // tolerate BOM

    if !text.starts_with("---") {
        return (SkillFrontmatter::default(), text.to_string());
    }

    // `splitn(3, "---")` yields ["", frontmatter, body...]. splitn(3) not split since body may contain `---`.
    let parts: Vec<&str> = text.splitn(3, "---").collect();
    if parts.len() < 3 {
        return (SkillFrontmatter::default(), text.to_string());
    }

    // parts[0] = "" before "---"; parts[1] = frontmatter; parts[2] = body (with leading newline).
    let fm_text = parts[1];
    let body = parts[2].trim_start_matches(['\r', '\n']).to_string();

    match serde_yaml::from_str::<SkillFrontmatter>(fm_text) {
        Ok(fm) => (fm, body),
        Err(_) => (SkillFrontmatter::default(), text.to_string()),
    }
}

/// Normalize description: strip leading `#`/whitespace, split on whitespace, join with single space.
/// Collapses `description: |` multi-line block scalars to one line; matches Python `" ".join(desc.lstrip("# ").split())`.
fn normalize_description(desc: &str) -> String {
    let trimmed = desc.trim_start_matches(['#', ' ', '\t']).trim();
    trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Temp skills root, cleaned up after the test.
    fn temp_skills_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bytemaker-skills-{}", label));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, dir: &str, content: &str) {
        let skill_dir = root.join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parse_frontmatter_name_and_desc() {
        let text = "---\nname: code-review\ndescription: Do code reviews.\n---\n# Code Review\nbody";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("code-review"));
        assert_eq!(fm.description.as_deref(), Some("Do code reviews."));
        assert_eq!(body, "# Code Review\nbody");
    }

    #[test]
    fn parse_frontmatter_block_scalar_description() {
        // agent-builder's `description: |` multi-line block scalar: serde_yaml parses it with newlines;
        // normalize_description collapses it to one line.
        let text = "---\nname: agent-builder\ndescription: |\n  Design agents.\n  Use when users ask.\n---\nbody";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("agent-builder"));
        let norm = normalize_description(fm.description.as_deref().unwrap_or(""));
        assert_eq!(norm, "Design agents. Use when users ask.");
        assert_eq!(body, "body");
    }

    #[test]
    fn parse_frontmatter_missing_falls_back_to_full_text() {
        let text = "# Just a heading\nno frontmatter here";
        let (fm, body) = parse_frontmatter(text);
        assert!(fm.name.is_none());
        assert!(fm.description.is_none());
        assert_eq!(body, text);
    }

    #[test]
    fn parse_frontmatter_malformed_yaml_falls_back() {
        // malformed YAML (mismatched key/value, invalid content after bare colon)
        let text = "---\nname: : :\n---\nbody";
        let (fm, body) = parse_frontmatter(text);
        assert!(fm.name.is_none());
        // on fallback, body = full text
        assert!(body.starts_with("---"));
    }

    #[test]
    fn parse_frontmatter_extra_fields_ignored() {
        let text = "---\nname: pdf\ndescription: Process PDFs.\nversion: 1.0\nauthor: bob\n---\nbody";
        let (fm, _) = parse_frontmatter(text);
        assert_eq!(fm.name.as_deref(), Some("pdf"));
        assert_eq!(fm.description.as_deref(), Some("Process PDFs."));
    }

    #[test]
    fn normalize_strips_hash_and_collapses_whitespace() {
        assert_eq!(normalize_description("#  Code   Review "), "Code Review");
        assert_eq!(normalize_description("  hello\nworld  "), "hello world");
        assert_eq!(normalize_description(""), "");
    }

    #[test]
    fn scan_collects_skills() {
        let root = temp_skills_root("scan-collects");
        write_skill(
            &root,
            "code-review",
            "---\nname: code-review\ndescription: Do code reviews.\n---\n# Code Review",
        );
        write_skill(
            &root,
            "pdf",
            "---\nname: pdf\ndescription: Process PDFs.\n---\n# PDF",
        );
        // a subdir without SKILL.md: should be skipped
        fs::create_dir_all(root.join("empty")).unwrap();

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.len(), 2);
        let cat = loader.catalog();
        assert!(cat.contains("- code-review: Do code reviews."));
        assert!(cat.contains("- pdf: Process PDFs."));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_uses_dir_name_when_frontmatter_name_absent() {
        let root = temp_skills_root("dir-name-fallback");
        write_skill(
            &root,
            "mcp-builder",
            "---\ndescription: Build MCP servers.\n---\n# MCP Builder",
        );

        let loader = SkillLoader::scan(root.clone());
        let skill = loader.skills.get("mcp-builder").expect("keyed by dir name");
        assert_eq!(skill.name, "mcp-builder");
        assert_eq!(skill.description, "Build MCP servers.");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_uses_first_body_line_when_description_absent() {
        let root = temp_skills_root("first-line-fallback");
        write_skill(&root, "misc", "---\nname: misc\n---\n# This is the heading\nbody");

        let loader = SkillLoader::scan(root.clone());
        let skill = loader.skills.get("misc").unwrap();
        assert_eq!(skill.description, "This is the heading");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_missing_dir_yields_empty() {
        let loader = SkillLoader::scan(PathBuf::from("/no/such/dir/here-xyz"));
        assert_eq!(loader.len(), 0);
        assert_eq!(loader.catalog(), "");
    }

    #[test]
    fn scan_not_recursive_into_references() {
        // references/ inside a skill subdir should not be treated as a skill
        let root = temp_skills_root("not-recursive");
        let skill_dir = root.join("agent-builder");
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: agent-builder\ndescription: Build agents.\n---\nbody",
        )
        .unwrap();
        // a fake SKILL.md in references/ would be collected by recursive scan, but this impl must not
        fs::write(
            skill_dir.join("references").join("SKILL.md"),
            "---\nname: phantom\ndescription: should not load.\n---\nbody",
        )
        .unwrap();

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.len(), 1);
        assert!(loader.skills.contains_key("agent-builder"));
        assert!(!loader.skills.contains_key("phantom"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_returns_full_content_on_hit() {
        let root = temp_skills_root("load-hit");
        let content = "---\nname: code-review\ndescription: Do code reviews.\n---\n# Code Review\n## Checklist\n- security";
        write_skill(&root, "code-review", content);

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.load("code-review"), content);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_returns_error_listing_available_on_miss() {
        let root = temp_skills_root("load-miss");
        write_skill(
            &root,
            "code-review",
            "---\nname: code-review\ndescription: Do code reviews.\n---\nbody",
        );
        write_skill(
            &root,
            "pdf",
            "---\nname: pdf\ndescription: Process PDFs.\n---\nbody",
        );

        let loader = SkillLoader::scan(root.clone());
        let got = loader.load("nonexistent");
        assert!(got.starts_with("Error: Unknown skill 'nonexistent'."));
        // BTreeMap order: code-review, pdf
        assert!(got.contains("Available: code-review, pdf"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_miss_when_no_skills_says_none() {
        let loader = SkillLoader::scan(PathBuf::from("/no/such/dir/here-xyz-empty"));
        let got = loader.load("whatever");
        assert!(got.contains("Available: none"));
    }

    #[test]
    fn duplicate_name_later_overwrites() {
        // Two skills with same frontmatter name: BTreeMap insert overwrites. Dir read order is non-deterministic,
        // but "only one remains, keyed by that name" is guaranteed.
        let root = temp_skills_root("dup-name");
        write_skill(
            &root,
            "a",
            "---\nname: same\ndescription: first.\n---\nfirst body",
        );
        write_skill(
            &root,
            "b",
            "---\nname: same\ndescription: second.\n---\nsecond body",
        );

        let loader = SkillLoader::scan(root.clone());
        assert_eq!(loader.len(), 1); // same-name merges into one
        let cat = loader.catalog();
        // one of the two, depending on scan order, but description matches one body
        assert!(
            cat.contains("first.") || cat.contains("second."),
            "catalog should contain one of the two: {}",
            cat
        );
        let _ = fs::remove_dir_all(&root);
    }
}
