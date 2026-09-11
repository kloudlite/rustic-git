//! The file tools: `read`, `write`, `edit`, `patch`, `glob`, `grep`. One implementation each, no
//! shelling out to `rg` or `find` — the semantics are the tool's, and they are testable against a
//! tempdir. Shaped to spend few tokens: `read` takes several paths in one call, `edit` takes
//! several files in one atomic call, `patch` takes a unified diff (smaller than old/new pairs for
//! a large rewrite), and every answer is a confirmation, never an echo of what was written.
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

/// One path, or `paths` for several in one call — each answered on its own, an unreadable one as
/// `{path, error}` beside the others rather than failing the batch.
pub fn read(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        if paths.is_empty() || paths.len() > 50 {
            return Err(ToolError::Invalid("`paths`: 1..=50 entries".into()));
        }
        let files: Vec<Value> = paths
            .iter()
            .map(|p| {
                let one = match p {
                    Value::String(s) => json!({ "path": s, "offset": args.get("offset"), "limit": args.get("limit") }),
                    other => other.clone(),
                };
                match read_one(root, home, &one) {
                    Ok(v) => v,
                    Err(e) => json!({ "path": one.get("path").cloned().unwrap_or(Value::Null), "error": e.to_string() }),
                }
            })
            .collect();
        return Ok(json!({ "files": files }));
    }
    read_one(root, home, args)
}

fn read_one(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
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

/// `{path, edits}` for one file or `{files: [{path, edits}]}` for several — ALL or nothing across
/// the whole call: every edit is applied to in-memory copies first, and only then is anything
/// written; a write that fails midway puts the files already written back.
pub fn edit(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let files: Vec<&Value> = match args.get("files").and_then(Value::as_array) {
        Some(f) if !f.is_empty() && f.len() <= 50 => f.iter().collect(),
        Some(_) => return Err(ToolError::Invalid("`files`: 1..=50 entries".into())),
        None => vec![args],
    };
    let mut staged: Vec<(PathBuf, String, String, usize)> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        let at = |m: &str| if files.len() > 1 { format!("file {fi}: {m}") } else { m.to_string() };
        let p = confine(root, home, str_arg(f, "path").map_err(|_| ToolError::Invalid(at("`path` (string) is required")))?)?;
        let edits = f.get("edits").and_then(Value::as_array).ok_or_else(|| ToolError::Invalid(at("`edits` (array of {old, new, replace_all?}) is required")))?;
        let before = std::fs::read_to_string(&p).map_err(|e| io(e, &p))?;
        let mut text = before.clone();
        // An edit that misses names its index so the caller can fix that one.
        for (i, e) in edits.iter().enumerate() {
            let old = str_arg(e, "old").map_err(|_| ToolError::Invalid(at(&format!("edit {i}: `old` (string) is required"))))?;
            let new = str_arg(e, "new").map_err(|_| ToolError::Invalid(at(&format!("edit {i}: `new` (string) is required"))))?;
            let n = text.matches(old).count();
            match (n, opt_bool(e, "replace_all")) {
                (0, _) => return Err(ToolError::Failed(at(&format!("edit {i}: `old` not found in {}", p.display())))),
                (1, _) | (_, true) => text = text.replace(old, new),
                (n, false) => return Err(ToolError::Failed(at(&format!("edit {i}: `old` matches {n} places in {}; add context or set replace_all", p.display())))),
            }
        }
        staged.push((p, before, text, edits.len()));
    }
    let mut written: Vec<(&PathBuf, &String)> = Vec::new();
    for (p, before, after, _) in &staged {
        if let Err(e) = atomic_write(p, after.as_bytes()) {
            for (wp, wb) in written {
                let _ = atomic_write(wp, wb.as_bytes());
            }
            return Err(e);
        }
        written.push((p, before));
    }
    if staged.len() == 1 {
        return Ok(json!({ "path": staged[0].0, "applied": staged[0].3 }));
    }
    Ok(json!({ "files": staged.iter().map(|(p, _, _, n)| json!({ "path": p, "applied": n })).collect::<Vec<_>>(), "applied": staged.iter().map(|s| s.3).sum::<usize>() }))
}

/// A unified diff applied to the tree — for a large rewrite a diff is fewer tokens than old/new
/// pairs, and it is what an agent already holds. `git apply` does the work (it needs no
/// repository, and it refuses a path that climbs out of the directory); `--check` first, so a
/// hunk that misses leaves nothing half-applied.
pub fn patch(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let diff = str_arg(args, "diff")?;
    if diff.len() as u64 > MAX_BYTES {
        return Err(ToolError::Failed(format!("{} bytes, over the {} byte limit", diff.len(), MAX_BYTES)));
    }
    let cwd = confine(root, home, opt_str(args, "cwd").unwrap_or("."))?;
    let run = |extra: &[&str]| -> Result<std::process::Output, ToolError> {
        use std::io::Write;
        let mut c = std::process::Command::new("git").args(["apply", "--whitespace=nowarn"]).args(extra).arg("-").current_dir(&cwd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| ToolError::Failed(format!("git apply: {e}")))?;
        c.stdin.take().ok_or_else(|| ToolError::Failed("git apply: no stdin".into()))?.write_all(diff.as_bytes()).map_err(|e| ToolError::Failed(format!("git apply: {e}")))?;
        c.wait_with_output().map_err(|e| ToolError::Failed(format!("git apply: {e}")))
    };
    let check = run(&["--check"])?;
    if !check.status.success() {
        return Err(ToolError::Failed(format!("patch does not apply: {}", String::from_utf8_lossy(&check.stderr).trim())));
    }
    let stat = run(&["--numstat"])?;
    let files: Vec<String> = String::from_utf8_lossy(&stat.stdout).lines().filter_map(|l| l.split('\t').nth(2).map(str::to_string)).collect();
    let applied = run(&[])?;
    if !applied.status.success() {
        return Err(ToolError::Failed(format!("git apply: {}", String::from_utf8_lossy(&applied.stderr).trim())));
    }
    Ok(json!({ "cwd": cwd, "files": files }))
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
            Tool { name: "read", description: "Read text files with line numbers. path for one file, or paths (up to 50 strings or {path, offset, limit} objects) for several in one call, each answered on its own. offset (1-based line) and limit (lines) page; a binary file answers its size and mime, no bytes.", schema: obj(json!({ "path": {"type":"string"}, "paths": {"type":"array"}, "offset": {"type":"integer"}, "limit": {"type":"integer"} }), &[]) },
            Tool { name: "write", description: "Create or overwrite a file atomically; parent directories are created.", schema: obj(json!({ "path": {"type":"string"}, "content": {"type":"string"} }), &["path","content"]) },
            Tool { name: "edit", description: "Exact string replacements, all or nothing across the whole call. {path, edits} for one file or files: [{path, edits}] (up to 50) for several. Each edit's `old` must occur exactly once unless replace_all.", schema: obj(json!({ "path": {"type":"string"}, "edits": {"type":"array","items":{"type":"object","properties":{"old":{"type":"string"},"new":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["old","new"]}}, "files": {"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"edits":{"type":"array"}},"required":["path","edits"]}} }), &[]) },
            Tool { name: "patch", description: "Apply a unified diff (git or plain -p1 format) under cwd (default: the workspace dir); checked first, so nothing is half-applied. Answers the files it touched. Fewer tokens than edit for a large rewrite.", schema: obj(json!({ "diff": {"type":"string"}, "cwd": {"type":"string"} }), &["diff"]) },
            Tool { name: "glob", description: "Files matching a glob under cwd (default: the workspace dir), gitignore-aware, newest first.", schema: obj(json!({ "pattern": {"type":"string"}, "cwd": {"type":"string"} }), &["pattern"]) },
            Tool { name: "grep", description: "Regex search under cwd, gitignore-aware. mode: content (default) | files | count; glob narrows files; context adds lines around a match.", schema: obj(json!({ "pattern": {"type":"string"}, "cwd": {"type":"string"}, "glob": {"type":"string"}, "mode": {"type":"string","enum":["content","files","count"]}, "context": {"type":"integer"}, "ignore_case": {"type":"boolean"}, "max": {"type":"integer"} }), &["pattern"]) },
        ]
    }

    fn call<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        async move {
            let (root, home, name) = (self.root.clone(), self.home.clone(), name.to_string());
            // File work is blocking I/O; off the reactor so a 10 MiB read never stalls a stream.
            let changes = matches!(name.as_str(), "write" | "edit" | "patch");
            let r = tokio::task::spawn_blocking(move || match name.as_str() {
                "read" => read(&root, &home, &args),
                "write" => write(&root, &home, &args),
                "edit" => edit(&root, &home, &args),
                "patch" => patch(&root, &home, &args),
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
    fn read_takes_several_paths_and_answers_each_on_its_own() {
        let (_t, root, home) = tree();
        let v = read(&root, &home, &json!({ "paths": ["README.md", { "path": "src/main.rs", "limit": 1 }, "missing.txt"], "limit": 1 })).unwrap();
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0]["content"], "     1\t# api");
        assert_eq!(files[1]["content"], "     1\tfn main() {");
        assert!(files[2]["error"].as_str().unwrap().contains("missing.txt"), "{v}");
        assert!(read(&root, &home, &json!({ "paths": [] })).is_err());
    }

    #[test]
    fn edit_across_files_is_atomic_and_patch_applies_a_unified_diff() {
        let (_t, root, home) = tree();
        let e = edit(&root, &home, &json!({ "files": [
            { "path": "README.md", "edits": [{ "old": "# api", "new": "# API" }] },
            { "path": "src/main.rs", "edits": [{ "old": "nope", "new": "x" }] }
        ] })).unwrap_err();
        assert!(e.to_string().starts_with("file 1: edit 0:"), "{e}");
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), "# api\n\nhello world\n", "the first file must not have landed");
        let v = edit(&root, &home, &json!({ "files": [
            { "path": "README.md", "edits": [{ "old": "# api", "new": "# API" }] },
            { "path": "src/main.rs", "edits": [{ "old": "hi", "new": "hello" }] }
        ] })).unwrap();
        assert_eq!(v["applied"], 2);
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), "# API\n\nhello world\n");
        let diff = "--- a/README.md\n+++ b/README.md\n@@ -1,3 +1,3 @@\n # API\n \n-hello world\n+hello there\n";
        let v = patch(&root, &home, &json!({ "diff": diff })).unwrap();
        assert_eq!(v["files"], json!(["README.md"]));
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), "# API\n\nhello there\n");
        let e = patch(&root, &home, &json!({ "diff": diff })).unwrap_err();
        assert!(e.to_string().contains("does not apply"), "{e}");
        let e = patch(&root, &home, &json!({ "diff": "--- a/../../x\n+++ b/../../x\n@@ -0,0 +1 @@\n+y\n" })).unwrap_err();
        assert!(e.to_string().contains("does not apply"), "a climbing path is refused by git apply: {e}");
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
