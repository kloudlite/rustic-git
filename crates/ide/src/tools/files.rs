//! The file tools: `read`, `write`, `edit`, `glob`, `grep`. One implementation each, no shelling
//! out to `rg` or `find` — the semantics are the tool's, and they are testable against a tempdir.
use super::{opt_bool, opt_str, opt_u64, str_arg, Tool, ToolError, ToolSet};
use crate::paths::confine;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAX_BYTES: u64 = 10 << 20;
pub const MAX_GREP: usize = 2_000;
pub const MAX_GLOB: usize = 5_000;

pub struct Files {
    pub root: PathBuf,
    pub home: PathBuf,
    /// Called after every `write` and `edit`: the graph refresh, wired by the server.
    pub after_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn io(e: std::io::Error, p: &Path) -> ToolError {
    ToolError::Failed(format!("{}: {e}", p.display()))
}

/// A file is binary when its first 8 KiB carry a NUL: the same rule `grep` and `git` use.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

pub fn read(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let p = confine(root, home, str_arg(args, "path")?)?;
    let meta = std::fs::metadata(&p).map_err(|e| io(e, &p))?;
    if meta.len() > MAX_BYTES {
        return Err(ToolError::Failed(format!("{}: {} bytes, over the {} byte read limit", p.display(), meta.len(), MAX_BYTES)));
    }
    let bytes = std::fs::read(&p).map_err(|e| io(e, &p))?;
    if looks_binary(&bytes) {
        return Ok(json!({ "path": p, "binary": true, "size": meta.len(), "mime": "application/octet-stream" }));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let offset = opt_u64(args, "offset").unwrap_or(1).max(1) as usize;
    let limit = opt_u64(args, "limit").unwrap_or(2_000) as usize;
    let slice: Vec<String> = lines.iter().skip(offset - 1).take(limit).enumerate().map(|(i, l)| format!("{:>6}\t{l}", offset + i)).collect();
    Ok(json!({ "path": p, "content": slice.join("\n"), "total_lines": lines.len(), "truncated": offset - 1 + slice.len() < lines.len() }))
}

pub fn write(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let p = confine(root, home, str_arg(args, "path")?)?;
    let content = str_arg(args, "content")?;
    if content.len() as u64 > MAX_BYTES {
        return Err(ToolError::Failed(format!("{} bytes, over the {} byte write limit", content.len(), MAX_BYTES)));
    }
    let created = !p.exists();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(e, parent))?;
    }
    atomic_write(&p, content.as_bytes())?;
    Ok(json!({ "path": p, "bytes": content.len(), "created": created }))
}

/// Temp file beside the target, then rename: a reader never sees a half-written file, and a
/// crash leaves the old content.
fn atomic_write(p: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let tmp = p.with_extension(format!("{}.kl-ide-tmp", p.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::write(&tmp, bytes).map_err(|e| io(e, &tmp))?;
    std::fs::rename(&tmp, p).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e, p)
    })
}

pub fn edit(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let p = confine(root, home, str_arg(args, "path")?)?;
    let edits = args.get("edits").and_then(Value::as_array).ok_or_else(|| ToolError::Invalid("`edits` (array of {old, new, replace_all?}) is required".into()))?;
    let mut text = std::fs::read_to_string(&p).map_err(|e| io(e, &p))?;
    // All or nothing: every edit is applied to the in-memory copy first, and the file is written
    // once. An edit that misses names its index so the caller can fix that one.
    for (i, e) in edits.iter().enumerate() {
        let old = str_arg(e, "old").map_err(|_| ToolError::Invalid(format!("edit {i}: `old` (string) is required")))?;
        let new = str_arg(e, "new").map_err(|_| ToolError::Invalid(format!("edit {i}: `new` (string) is required")))?;
        let n = text.matches(old).count();
        match (n, opt_bool(e, "replace_all")) {
            (0, _) => return Err(ToolError::Failed(format!("edit {i}: `old` not found in {}", p.display()))),
            (1, _) | (_, true) => text = text.replace(old, new),
            (n, false) => return Err(ToolError::Failed(format!("edit {i}: `old` matches {n} places in {}; add context or set replace_all", p.display()))),
        }
    }
    atomic_write(&p, text.as_bytes())?;
    Ok(json!({ "path": p, "applied": edits.len() }))
}

pub fn glob(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let pattern = str_arg(args, "pattern")?;
    let cwd = confine(root, home, opt_str(args, "cwd").unwrap_or("."))?;
    let matcher = globset::GlobBuilder::new(pattern).literal_separator(false).build().map_err(|e| ToolError::Invalid(format!("pattern: {e}")))?.compile_matcher();
    let mut hits: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut truncated = false;
    for entry in ignore::WalkBuilder::new(&cwd).hidden(false).require_git(false).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(&cwd).unwrap_or(entry.path());
        if matcher.is_match(rel) {
            if hits.len() >= MAX_GLOB {
                truncated = true;
                break;
            }
            let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
            hits.push((mtime, rel.to_path_buf()));
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    Ok(json!({ "cwd": cwd, "paths": hits.into_iter().map(|(_, p)| p).collect::<Vec<_>>(), "truncated": truncated }))
}

pub fn grep(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let pattern = str_arg(args, "pattern")?;
    let cwd = confine(root, home, opt_str(args, "cwd").unwrap_or("."))?;
    let mode = opt_str(args, "mode").unwrap_or("content");
    let context = opt_u64(args, "context").unwrap_or(0) as usize;
    let max = opt_u64(args, "max").map(|m| m as usize).unwrap_or(MAX_GREP).min(MAX_GREP);
    let re = regex::RegexBuilder::new(pattern).case_insensitive(opt_bool(args, "ignore_case")).build().map_err(|e| ToolError::Invalid(format!("pattern: {e}")))?;
    let only = opt_str(args, "glob").map(|g| globset::GlobBuilder::new(g).literal_separator(false).build().map_err(|e| ToolError::Invalid(format!("glob: {e}")))).transpose()?.map(|g| g.compile_matcher());
    let mut matches = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    let mut counts: Vec<Value> = Vec::new();
    let mut truncated = false;
    'files: for entry in ignore::WalkBuilder::new(&cwd).hidden(false).require_git(false).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(&cwd).unwrap_or(entry.path()).to_path_buf();
        if only.as_ref().is_some_and(|m| !m.is_match(&rel)) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        if looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let mut n = 0usize;
        for (i, l) in lines.iter().enumerate() {
            if !re.is_match(l) {
                continue;
            }
            n += 1;
            if mode == "content" {
                if matches.len() >= max {
                    truncated = true;
                    break 'files;
                }
                let lo = i.saturating_sub(context);
                let hi = (i + context + 1).min(lines.len());
                matches.push(json!({ "path": rel, "line": i + 1, "text": l, "context": if context > 0 { Some(lines[lo..hi].join("\n")) } else { None } }));
            }
        }
        if n > 0 {
            files.push(json!(rel));
            counts.push(json!({ "path": rel, "count": n }));
            if mode != "content" && files.len() >= max {
                truncated = true;
                break;
            }
        }
    }
    Ok(match mode {
        "files" => json!({ "cwd": cwd, "files": files, "truncated": truncated }),
        "count" => json!({ "cwd": cwd, "counts": counts, "truncated": truncated }),
        _ => json!({ "cwd": cwd, "matches": matches, "truncated": truncated }),
    })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

impl ToolSet for Files {
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool { name: "read", description: "Read a text file with line numbers. offset (1-based line) and limit (lines) page it; a binary file answers its size and mime, no bytes.", schema: obj(json!({ "path": {"type":"string"}, "offset": {"type":"integer"}, "limit": {"type":"integer"} }), &["path"]) },
            Tool { name: "write", description: "Create or overwrite a file atomically; parent directories are created.", schema: obj(json!({ "path": {"type":"string"}, "content": {"type":"string"} }), &["path","content"]) },
            Tool { name: "edit", description: "Apply exact string replacements, all or nothing. Each edit's `old` must occur exactly once unless replace_all.", schema: obj(json!({ "path": {"type":"string"}, "edits": {"type":"array","items":{"type":"object","properties":{"old":{"type":"string"},"new":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["old","new"]}} }), &["path","edits"]) },
            Tool { name: "glob", description: "Files matching a glob under cwd (default: the workspace dir), gitignore-aware, newest first.", schema: obj(json!({ "pattern": {"type":"string"}, "cwd": {"type":"string"} }), &["pattern"]) },
            Tool { name: "grep", description: "Regex search under cwd, gitignore-aware. mode: content (default) | files | count; glob narrows files; context adds lines around a match.", schema: obj(json!({ "pattern": {"type":"string"}, "cwd": {"type":"string"}, "glob": {"type":"string"}, "mode": {"type":"string","enum":["content","files","count"]}, "context": {"type":"integer"}, "ignore_case": {"type":"boolean"}, "max": {"type":"integer"} }), &["pattern"]) },
        ]
    }

    fn call<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        async move {
            let (root, home, name) = (self.root.clone(), self.home.clone(), name.to_string());
            // File work is blocking I/O; off the reactor so a 10 MiB read never stalls a stream.
            let changes = matches!(name.as_str(), "write" | "edit");
            let r = tokio::task::spawn_blocking(move || match name.as_str() {
                "read" => read(&root, &home, &args),
                "write" => write(&root, &home, &args),
                "edit" => edit(&root, &home, &args),
                "glob" => glob(&root, &home, &args),
                "grep" => grep(&root, &home, &args),
                other => Err(ToolError::Unknown(other.to_string())),
            })
            .await
            .map_err(|e| ToolError::Failed(format!("task: {e}")))?;
            if changes && r.is_ok() {
                if let Some(f) = &self.after_change {
                    f();
                }
            }
            r
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("workspaces/api");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        std::fs::write(root.join("README.md"), "# api\n\nhello world\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/out.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("blob.bin"), b"\x00\x01\x02binary").unwrap();
        (tmp, root, home)
    }

    #[test]
    fn read_pages_by_line_and_sniffs_binary() {
        let (_t, root, home) = tree();
        let v = read(&root, &home, &json!({ "path": "src/main.rs", "offset": 2, "limit": 1 })).unwrap();
        assert_eq!(v["content"], "     2\t    println!(\"hi\");");
        assert_eq!(v["total_lines"], 3);
        assert_eq!(v["truncated"], true);
        let b = read(&root, &home, &json!({ "path": "blob.bin" })).unwrap();
        assert_eq!(b["binary"], true);
        assert!(read(&root, &home, &json!({ "path": "../../etc/passwd" })).is_err());
    }

    #[test]
    fn write_creates_parents_and_reports_created() {
        let (_t, root, home) = tree();
        let v = write(&root, &home, &json!({ "path": "deep/er/new.txt", "content": "x" })).unwrap();
        assert_eq!(v["created"], true);
        let v = write(&root, &home, &json!({ "path": "deep/er/new.txt", "content": "xy" })).unwrap();
        assert_eq!(v["created"], false);
        assert_eq!(std::fs::read_to_string(root.join("deep/er/new.txt")).unwrap(), "xy");
        assert!(!root.join("deep/er/new.kl-ide-tmp").exists() && std::fs::read_dir(root.join("deep/er")).unwrap().count() == 1, "no temp file left behind");
    }

    #[test]
    fn edit_is_all_or_nothing_and_names_the_failing_index() {
        let (_t, root, home) = tree();
        let e = edit(&root, &home, &json!({ "path": "src/main.rs", "edits": [{ "old": "hi", "new": "hello" }, { "old": "nope", "new": "x" }] })).unwrap_err();
        assert!(e.to_string().starts_with("edit 1:"), "{e}");
        assert!(std::fs::read_to_string(root.join("src/main.rs")).unwrap().contains("\"hi\""), "first edit must not have landed");
        let e = edit(&root, &home, &json!({ "path": "README.md", "edits": [{ "old": "l", "new": "L" }] })).unwrap_err();
        assert!(e.to_string().contains("matches"), "{e}");
        let v = edit(&root, &home, &json!({ "path": "README.md", "edits": [{ "old": "l", "new": "L", "replace_all": true }] })).unwrap();
        assert_eq!(v["applied"], 1);
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), "# api\n\nheLLo worLd\n");
    }

    #[test]
    fn glob_and_grep_respect_gitignore_and_modes() {
        let (_t, root, home) = tree();
        let v = glob(&root, &home, &json!({ "pattern": "**/*.rs" })).unwrap();
        let paths: Vec<String> = v["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
        assert_eq!(paths, vec!["src/main.rs"], "target/ is ignored");
        let v = grep(&root, &home, &json!({ "pattern": "fn main", "mode": "files" })).unwrap();
        assert_eq!(v["files"].as_array().unwrap().len(), 1);
        let v = grep(&root, &home, &json!({ "pattern": "HELLO", "ignore_case": true })).unwrap();
        assert_eq!(v["matches"][0]["line"], 3);
        let v = grep(&root, &home, &json!({ "pattern": "println", "context": 1 })).unwrap();
        assert!(v["matches"][0]["context"].as_str().unwrap().contains("fn main()"));
        let v = grep(&root, &home, &json!({ "pattern": "a", "mode": "count", "glob": "*.md" })).unwrap();
        assert_eq!(v["counts"].as_array().unwrap().len(), 1);
    }
}
