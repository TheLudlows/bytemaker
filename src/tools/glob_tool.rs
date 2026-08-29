// tools/glob_tool - Glob tool implementation
//
// Uses the `glob` crate for file system pattern matching.
// Supports patterns like **/*.rs, src/**/*.ts, *.txt.
// Returns matching file paths as forward-slash-separated relative paths.

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

/// Max result count to cap very large repos.
const MAX_RESULTS: usize = 5000;

/// Normalize path separators to / (Windows strip_prefix yields \).
fn to_unix_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// Collect relative paths (/-separated) under base matching the glob pattern.
/// Capped at MAX_RESULTS.
pub(crate) fn glob_in(pattern: &str, base: &Path) -> Vec<String> {
    let full_pattern = base.join(pattern).to_string_lossy().to_string();

    let mut results: Vec<String> = Vec::new();

    let paths = match glob::glob(&full_pattern) {
        Ok(paths) => paths,
        Err(e) => {
            tracing::warn!("glob pattern error: {}", e);
            return results;
        }
    };

    for entry in paths {
        match entry {
            Ok(path) => {
                if let Ok(rel) = path.strip_prefix(base) {
                    let rel = to_unix_path(&rel.to_string_lossy());
                    results.push(rel);
                    if results.len() >= MAX_RESULTS {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("glob entry error: {}", e);
            }
        }
    }

    results
}

/// Find matching files (glob rules, recursive over workspace).
pub(crate) fn run_glob(pattern: &str, base: &Path) -> String {
    let results = glob_in(pattern, base);
    if results.is_empty() {
        "Error: no matches".to_string()
    } else {
        results.join("\n")
    }
}

/// Glob tool for file system pattern matching
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool that works with any codebase size. Supports glob patterns like **/*.js or src/**/*.ts. Returns matching file paths."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter undefined or null - simply omit it for the default behavior. Must be a valid directory path if provided."
                }
            },
            "required": ["pattern"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let pattern = input["pattern"].as_str().unwrap_or("");

        let search_path = input["path"].as_str().map(|s| s.to_string());

        if let Some(path) = search_path {
            let results = glob_in(pattern, Path::new(&path));
            if results.is_empty() {
                "Error: no matches".to_string()
            } else {
                results.join("\n")
            }
        } else {
            let cwd = match crate::tools::ctx_cwd(ctx) {
                Ok(p) => p,
                Err(e) => return format!("Error: {}", e),
            };
            run_glob(pattern, &cwd)
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_glob_tool_name() {
        let tool = GlobTool;
        assert_eq!(tool.name(), "glob");
    }

    #[test]
    fn test_glob_tool_description() {
        let tool = GlobTool;
        assert!(tool.description().contains("pattern matching"));
    }

    #[test]
    fn test_glob_tool_schema() {
        let tool = GlobTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["pattern"]["type"], "string");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "pattern");
    }

    #[test]
    fn test_glob_permission_always_pass() {
        let tool = GlobTool;
        match tool.check_permission(&serde_json::json!({"pattern": "**/*"})) {
            PermissionCheck::Pass => {}
            _ => panic!("Glob should always pass permission check"),
        }
    }

    #[test]
    fn glob_in_finds_matching_files() {
        let dir = std::env::temp_dir().join("bytemaker-glob-test-walk");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("a.rs"), b"").unwrap();
        fs::write(dir.join("b.txt"), b"").unwrap();
        fs::write(dir.join("src").join("c.rs"), b"").unwrap();

        let mut got = glob_in("**/*.rs", &dir);
        got.sort();
        let mut want = vec!["a.rs".to_string(), "src/c.rs".to_string()];
        want.sort();
        assert_eq!(got, want);

        let top = glob_in("*.rs", &dir);
        assert_eq!(top, vec!["a.rs".to_string()]);

        assert_eq!(glob_in("zzz", &dir), Vec::<String>::new());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_in_single_level_pattern() {
        let dir = std::env::temp_dir().join("bytemaker-glob-test-single");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("x.txt"), b"").unwrap();
        fs::write(dir.join("y.rs"), b"").unwrap();
        fs::write(dir.join("sub").join("z.txt"), b"").unwrap();

        let got = glob_in("*.txt", &dir);
        assert_eq!(got, vec!["x.txt".to_string()]);

        let mut all = glob_in("**/*", &dir);
        all.sort();
        assert!(all.contains(&"x.txt".to_string()));
        assert!(all.contains(&"y.rs".to_string()));
        assert!(all.contains(&"sub/z.txt".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_in_empty_directory() {
        let dir = std::env::temp_dir().join("bytemaker-glob-test-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let got = glob_in("**/*.rs", &dir);
        assert!(got.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
