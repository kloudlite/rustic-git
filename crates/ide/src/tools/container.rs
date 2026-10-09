//! `container_build` and `container_push`: `kl container build|push` run by the tool server itself,
//! NOT inside the exec sandbox.
//!
//! Why outside: the sandbox (`crate::sandbox`) binds only `/nix` and a few `/etc` files, so `kl`,
//! `docker-credential-kl` (both in `/usr/local/bin`) and the token under `/etc/kloudlite` are
//! invisible to an agent's `exec` — deliberately, the token must not be readable by agent code.
//! A session asked to "build the image" found no `kl` on PATH (2026-10-09). These two tools hand the
//! agent the verb without the credential: the server runs `kl` with its own environment, and the
//! only thing the caller chooses is WHAT to build, confined to its tree (`paths::confine`), so a
//! build context or Dockerfile outside the tree is refused before `kl` starts.
//!
//! Output defaults to the last 40 lines of each stream: a buildx log is thousands of lines and the
//! tail is where the error is.
use super::exec::{job, trim_output, MAX_TIMEOUT_MS};
use super::{opt_bool, opt_str, opt_u64, str_arg, Tool, ToolError, ToolSet};
use crate::paths::confine;
use crate::trees::Trees;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::process::Command;

const DEFAULT_TAIL: u64 = 40;

pub struct Container {
    pub trees: Arc<Trees>,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// A required or optional array of strings; anything else is the caller's mistake, said so.
fn strings(args: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| ToolError::Invalid(format!("{key}: strings only")))).collect(),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        _ => Err(ToolError::Invalid(format!("{key}: an array of strings"))),
    }
}

/// `kl container build` argv, everything after `kl`. `context` and `file` arrive already confined
/// and absolute. `--` before the context: it is whatever the caller typed.
fn build_argv(tags: &[String], file: Option<&str>, build_args: &[String], platform: Option<&str>, no_cache: bool, context: &str) -> Vec<String> {
    let mut v = vec!["container".to_string(), "build".to_string()];
    for t in tags {
        v.extend(["-t".to_string(), t.clone()]);
    }
    if let Some(f) = file {
        v.extend(["-f".to_string(), f.to_string()]);
    }
    for a in build_args {
        v.extend(["--build-arg".to_string(), a.clone()]);
    }
    if let Some(p) = platform {
        v.extend(["--platform".to_string(), p.to_string()]);
    }
    if no_cache {
        v.push("--no-cache".to_string());
    }
    v.extend(["--".to_string(), context.to_string()]);
    v
}

impl ToolSet for Container {
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool { name: "container_build", description: "Build a container image on your builder and push it to your registry (`kl container build`). Each of `tags` is `name[:tag]`. `hello:1` means `<registry>/<you>/hello:1`. `team/hello:1` pushes the image under the team. `context` and `file` are paths in your working directory. The default for `context` is \".\". The tool waits for the build (`timeout_ms`). The default is 600000. The maximum is 600000. It returns `exit_code`, `stdout`, and `stderr`. Each stream keeps its last 40 lines, unless `head` or `tail` says otherwise.", schema: obj(json!({ "tree": {"type":"string"}, "tags": {"type":"array","items":{"type":"string"}}, "context": {"type":"string"}, "file": {"type":"string"}, "build_args": {"type":"array","items":{"type":"string"}}, "platform": {"type":"string"}, "no_cache": {"type":"boolean"}, "timeout_ms": {"type":"integer"}, "head": {"type":"integer"}, "tail": {"type":"integer"} }), &["tags"]) },
            Tool { name: "container_push", description: "Copy an image that your registry already has to more names. The tool does not rebuild the image (`kl container push src dst...`). For example, `src` is `hello:1` and `dst` is `[hello:latest]`.", schema: obj(json!({ "tree": {"type":"string"}, "src": {"type":"string"}, "dst": {"type":"array","items":{"type":"string"}} }), &["src","dst"]) },
        ]
    }

    fn call<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        async move {
            let t = self.trees.resolve(opt_str(&args, "tree"))?;
            let argv = match name {
                "container_build" => {
                    let tags = strings(&args, "tags")?;
                    if tags.is_empty() {
                        return Err(ToolError::Invalid("tags: at least one".into()));
                    }
                    let context = confine(&t, opt_str(&args, "context").unwrap_or("."))?;
                    let file = opt_str(&args, "file").map(|f| confine(&t, f)).transpose()?;
                    build_argv(&tags, file.as_deref().and_then(|p| p.to_str()), &strings(&args, "build_args")?, opt_str(&args, "platform"), opt_bool(&args, "no_cache"), &context.to_string_lossy())
                }
                "container_push" => {
                    let dst = strings(&args, "dst")?;
                    if dst.is_empty() {
                        return Err(ToolError::Invalid("dst: at least one".into()));
                    }
                    let mut v = vec!["container".to_string(), "push".to_string(), "--".to_string(), str_arg(&args, "src")?.to_string()];
                    v.extend(dst);
                    v
                }
                other => return Err(ToolError::Unknown(other.to_string())),
            };
            let mut cmd = Command::new("kl");
            cmd.args(&argv).current_dir(&t.root).process_group(0).kill_on_drop(true);
            let timeout = opt_u64(&args, "timeout_ms").unwrap_or(MAX_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
            let mut shape = args.clone();
            if shape.get("head").is_none() && shape.get("tail").is_none() {
                shape["tail"] = json!(DEFAULT_TAIL);
            }
            job(cmd, timeout).await.map(|v| trim_output(v, &shape))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> (tempfile::TempDir, Container) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        (tmp, Container { trees: Arc::new(Trees::new(root, None)) })
    }

    #[test]
    fn build_argv_passes_every_flag_and_ends_with_the_context() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            build_argv(&s(&["hello:1", "hello:latest"]), Some("/w/Dockerfile"), &s(&["A=1"]), Some("linux/amd64"), true, "/w"),
            s(&["container", "build", "-t", "hello:1", "-t", "hello:latest", "-f", "/w/Dockerfile", "--build-arg", "A=1", "--platform", "linux/amd64", "--no-cache", "--", "/w"])
        );
        assert_eq!(build_argv(&s(&["x"]), None, &[], None, false, "/w"), s(&["container", "build", "-t", "x", "--", "/w"]));
    }

    #[tokio::test]
    async fn a_context_or_file_outside_the_tree_is_refused_before_kl_runs() {
        let (_t, c) = set();
        for args in [json!({ "tags": ["x"], "context": "../.." }), json!({ "tags": ["x"], "file": "../Dockerfile" }), json!({ "tags": ["x"], "context": "/etc" })] {
            assert!(matches!(c.call("container_build", args).await, Err(ToolError::Denied(_) | ToolError::Invalid(_))));
        }
        assert!(matches!(c.call("container_build", json!({ "tags": [] })).await, Err(ToolError::Invalid(_))));
        assert!(matches!(c.call("container_push", json!({ "src": "a:1", "dst": [] })).await, Err(ToolError::Invalid(_))));
    }
}
