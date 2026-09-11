//! `patch`: a unified diff applied in Rust — no `git apply`, no `patch` binary — so the file tools
//! are one process end to end and testable against a tempdir. Parse every file's hunks, apply them
//! all in memory (a hunk is placed at its stated line, else at the nearest place its context
//! matches within `FUZZ_LINES`), and only then write; a hunk that fits nowhere names its file and
//! line and nothing is written. Handles `a/` `b/` prefixes, `/dev/null` (a new or deleted file)
//! and `\ No newline at end of file`.
use super::files::atomic_write;
use super::ToolError;
use crate::paths::confine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How far from its stated line a hunk may land; a diff made against a slightly older copy still
/// applies, a diff made against another file does not.
pub const FUZZ_LINES: usize = 200;

#[derive(Debug, Default)]
struct Hunk {
    old_start: usize,
    lines: Vec<(char, String)>,
}

#[derive(Debug, Default)]
struct FilePatch {
    old: Option<String>,
    new: Option<String>,
    hunks: Vec<Hunk>,
}

fn strip_prefix(p: &str) -> Option<String> {
    let p = p.split('\t').next().unwrap_or(p).trim();
    if p == "/dev/null" {
        return None;
    }
    Some(p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string())
}

fn parse(diff: &str) -> Result<Vec<FilePatch>, ToolError> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = diff.lines().peekable();
    while let Some(l) = lines.next() {
        if let Some(old) = l.strip_prefix("--- ") {
            let Some(newl) = lines.next().and_then(|n| n.strip_prefix("+++ ")) else { return Err(ToolError::Invalid("patch: `---` without `+++`".into())) };
            files.push(FilePatch { old: strip_prefix(old), new: strip_prefix(newl), hunks: Vec::new() });
            continue;
        }
        if let Some(h) = l.strip_prefix("@@ ") {
            let Some(file) = files.last_mut() else { return Err(ToolError::Invalid("patch: a hunk before any file header".into())) };
            let mut ranges = h.split(' ');
            let range = |r: Option<&str>, sign: char| -> Result<(usize, usize), ToolError> {
                let r = r.unwrap_or("").trim_start_matches(sign);
                let (start, count) = r.split_once(',').unwrap_or((r, "1"));
                Ok((start.parse().map_err(|_| ToolError::Invalid(format!("patch: bad hunk header `{l}`")))?, count.parse().map_err(|_| ToolError::Invalid(format!("patch: bad hunk header `{l}`")))?))
            };
            let (old_start, old_count) = range(ranges.next(), '-')?;
            let (_, new_count) = range(ranges.next(), '+')?;
            let mut hunk = Hunk { old_start, lines: Vec::new() };
            let (mut seen_old, mut seen_new) = (0usize, 0usize);
            // The header's counts say where the hunk ends; a `---` of the next file is never a
            // removed line, however much it looks like one.
            while let Some(&n) = lines.peek() {
                if seen_old >= old_count && seen_new >= new_count && !n.starts_with('\\') {
                    break;
                }
                match n.chars().next() {
                    Some(' ') | Some('-') | Some('+') => {
                        let (k, rest) = n.split_at(1);
                        match k {
                            "-" => seen_old += 1,
                            "+" => seen_new += 1,
                            _ => {
                                seen_old += 1;
                                seen_new += 1;
                            }
                        }
                        hunk.lines.push((k.chars().next().unwrap_or(' '), rest.to_string()));
                        lines.next();
                    }
                    Some('\\') => {
                        // "\ No newline at end of file" qualifies the previous line.
                        if let Some(last) = hunk.lines.last_mut() {
                            last.1.push('\u{0}');
                        }
                        lines.next();
                    }
                    None => {
                        // An empty line inside a hunk is a context line whose content is empty.
                        seen_old += 1;
                        seen_new += 1;
                        hunk.lines.push((' ', String::new()));
                        lines.next();
                    }
                    _ => break,
                }
            }
            file.hunks.push(hunk);
        }
        // Anything else (`diff --git`, `index`, `new file mode`) is a header we do not need.
    }
    if files.is_empty() {
        return Err(ToolError::Invalid("patch: no `---`/`+++` file header".into()));
    }
    Ok(files)
}

/// Text as lines plus whether it ended with a newline, so the join puts back exactly what was there.
fn split(text: &str) -> (Vec<String>, bool) {
    let nl = text.ends_with('\n');
    let mut v: Vec<String> = text.split('\n').map(str::to_string).collect();
    if nl {
        v.pop();
    }
    (v, nl)
}

fn matches_at(doc: &[String], at: usize, expect: &[&str]) -> bool {
    at + expect.len() <= doc.len() && expect.iter().enumerate().all(|(i, e)| doc[at + i] == *e)
}

/// Apply one file's hunks to its lines. Answers the new lines and whether the file ends in a newline.
fn apply_file(mut doc: Vec<String>, mut trailing_nl: bool, fp: &FilePatch, name: &str) -> Result<(Vec<String>, bool), ToolError> {
    let mut offset: isize = 0;
    for (hi, h) in fp.hunks.iter().enumerate() {
        let expect: Vec<&str> = h.lines.iter().filter(|(k, _)| *k != '+').map(|(_, s)| s.trim_end_matches('\u{0}')).collect();
        let stated = (h.old_start.max(1) as isize - 1 + offset).max(0) as usize;
        let mut at = None;
        if matches_at(&doc, stated, &expect) {
            at = Some(stated);
        } else {
            for d in 1..=FUZZ_LINES {
                if stated >= d && matches_at(&doc, stated - d, &expect) {
                    at = Some(stated - d);
                    break;
                }
                if matches_at(&doc, stated + d, &expect) {
                    at = Some(stated + d);
                    break;
                }
            }
        }
        let Some(at) = at else { return Err(ToolError::Failed(format!("patch: hunk {} of {name} does not apply at line {}", hi + 1, h.old_start))) };
        let replacement: Vec<String> = h.lines.iter().filter(|(k, _)| *k != '-').map(|(_, s)| s.trim_end_matches('\u{0}').to_string()).collect();
        if let Some((_, last)) = h.lines.iter().rev().find(|(k, _)| *k != '-') {
            // The new side's last line says whether the file ends in a newline now.
            if at + expect.len() >= doc.len() {
                trailing_nl = !last.ends_with('\u{0}');
            }
        }
        doc.splice(at..at + expect.len(), replacement.iter().cloned());
        offset += replacement.len() as isize - expect.len() as isize;
    }
    Ok((doc, trailing_nl))
}

pub fn patch(root: &Path, home: &Path, args: &Value) -> Result<Value, ToolError> {
    let diff = super::str_arg(args, "diff")?;
    if diff.len() as u64 > super::files::MAX_BYTES {
        return Err(ToolError::Failed(format!("{} bytes, over the {} byte limit", diff.len(), super::files::MAX_BYTES)));
    }
    let cwd = confine(root, home, super::opt_str(args, "cwd").unwrap_or("."))?;
    let files = parse(diff)?;
    // Everything in memory first; a hunk that misses anywhere means nothing is written.
    let mut staged: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    for fp in &files {
        let target = fp.new.clone().or_else(|| fp.old.clone()).ok_or_else(|| ToolError::Invalid("patch: a file header with /dev/null on both sides".into()))?;
        let path = confine(&cwd, home, &target)?;
        let name = target.clone();
        let (doc, nl) = match &fp.old {
            None => (Vec::new(), true),
            Some(_) => {
                let text = std::fs::read_to_string(&path).map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
                split(&text)
            }
        };
        if fp.new.is_none() {
            // A deletion: the hunk must still match what is there.
            apply_file(doc, nl, fp, &name)?;
            staged.push((path, None));
        } else {
            let (out, nl) = apply_file(doc, nl, fp, &name)?;
            let mut text = out.join("\n");
            if nl && !out.is_empty() {
                text.push('\n');
            }
            staged.push((path, Some(text)));
        }
        touched.push(name);
    }
    for (path, content) in &staged {
        match content {
            Some(c) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| ToolError::Failed(format!("{}: {e}", parent.display())))?;
                }
                atomic_write(path, c.as_bytes())?;
            }
            None => std::fs::remove_file(path).map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?,
        }
    }
    Ok(json!({ "cwd": cwd, "files": touched }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        (tmp, root, home)
    }

    #[test]
    fn a_hunk_applies_at_its_line_and_a_missing_one_writes_nothing() {
        let (_t, root, home) = ws();
        let d = "--- a/a.txt\n+++ b/a.txt\n@@ -1,4 +1,4 @@\n one\n-two\n+TWO\n three\n four\n";
        let v = patch(&root, &home, &json!({ "diff": d })).unwrap();
        assert_eq!(v["files"], json!(["a.txt"]));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\nTWO\nthree\nfour\n");
        let e = patch(&root, &home, &json!({ "diff": d })).unwrap_err();
        assert!(e.to_string().contains("hunk 1 of a.txt does not apply"), "{e}");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\nTWO\nthree\nfour\n");
    }

    #[test]
    fn a_shifted_hunk_is_found_by_its_context_and_two_files_are_all_or_nothing() {
        let (_t, root, home) = ws();
        std::fs::write(root.join("a.txt"), "zero\none\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(root.join("b.txt"), "x\n").unwrap();
        let d = "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-nope\n+y\n";
        let e = patch(&root, &home, &json!({ "diff": d })).unwrap_err();
        assert!(e.to_string().contains("b.txt"), "{e}");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "zero\none\ntwo\nthree\nfour\n", "the first file must not have landed");
        let d = "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n";
        let v = patch(&root, &home, &json!({ "diff": d })).unwrap();
        assert_eq!(v["files"], json!(["a.txt", "b.txt"]));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "zero\none\nTWO\nthree\nfour\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "y\n");
    }

    #[test]
    fn new_deleted_and_no_trailing_newline_files_round_trip_and_a_climb_is_refused() {
        let (_t, root, home) = ws();
        let d = "diff --git a/new/n.txt b/new/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new/n.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n\\ No newline at end of file\n";
        patch(&root, &home, &json!({ "diff": d })).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("new/n.txt")).unwrap(), "hello\nworld");
        let d = "--- a/new/n.txt\n+++ b/new/n.txt\n@@ -1,2 +1,2 @@\n hello\n-world\n\\ No newline at end of file\n+world\n";
        patch(&root, &home, &json!({ "diff": d })).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("new/n.txt")).unwrap(), "hello\nworld\n");
        let d = "--- a/new/n.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-hello\n-world\n";
        patch(&root, &home, &json!({ "diff": d })).unwrap();
        assert!(!root.join("new/n.txt").exists());
        let e = patch(&root, &home, &json!({ "diff": "--- a/../../x\n+++ b/../../x\n@@ -0,0 +1 @@\n+y\n" })).unwrap_err();
        assert!(matches!(e, ToolError::Denied(_)), "{e}");
        assert!(matches!(parse("garbage"), Err(ToolError::Invalid(_))));
    }
}
