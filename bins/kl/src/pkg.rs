//! `kl pkg` — the declared package list of THIS workspace, edited from inside it. The api owns
//! validation and locking; this file only merges the list and prints what came back, so a
//! refusal a person reads is the api's own sentence.

use serde_json::{json, Value};

use crate::api::Api;

/// The attribute half of an entry: `nodejs@20` and `nodejs` are the same package.
fn attr(entry: &str) -> &str {
    entry.split('@').next().unwrap_or(entry)
}

/// Added entries replace the same attr in place (a re-pin is an edit, not a duplicate) and are
/// appended otherwise. The last spelling of a repeated argument wins.
pub fn merge(existing: &[String], add: &[String]) -> Vec<String> {
    let mut out = existing.to_vec();
    for e in add {
        match out.iter().position(|x| attr(x) == attr(e)) {
            Some(i) => out[i] = e.clone(),
            None => out.push(e.clone()),
        }
    }
    out
}

/// By attr, so `kl pkg rm nodejs@20` removes `nodejs` whatever it is pinned at.
pub fn remove(existing: &[String], rm: &[String]) -> Result<Vec<String>, String> {
    if let Some(m) = rm.iter().find(|e| !existing.iter().any(|x| attr(x) == attr(e))) {
        return Err(format!("{}: not in the package list", attr(m)));
    }
    Ok(existing.iter().filter(|x| !rm.iter().any(|e| attr(e) == attr(x))).cloned().collect())
}

fn packages(doc: &Value) -> Vec<String> {
    doc.get("packages").and_then(|p| p.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// `locks[].version` for an entry, when the api resolved one.
fn locked(doc: &Value, entry: &str) -> Option<String> {
    doc.get("locks")?.as_array()?.iter().find(|l| l.get("entry").and_then(|e| e.as_str()) == Some(entry))?.get("version")?.as_str().map(str::to_string)
}

fn line(doc: &Value, entry: &str, verb: &str) {
    match locked(doc, entry) {
        Some(v) => println!("{verb}{entry} (locked {v})"),
        None => println!("{verb}{entry}"),
    }
}

const REBUILD: &str = "the workspace rebuilds its profile; new binaries appear in a fresh shell";

fn ws_path(id: &str) -> String {
    format!("/v1/workspaces/{id}")
}

pub fn list(api: &Api, id: &str) -> Result<(), String> {
    let doc = api.get(&ws_path(id))?;
    let list = packages(&doc);
    if list.is_empty() {
        println!("no packages declared — `kl pkg add jq`");
        return Ok(());
    }
    for e in list {
        line(&doc, &e, "");
    }
    Ok(())
}

fn patch(api: &Api, id: &str, packages: Vec<String>) -> Result<Value, String> {
    api.patch_json(&ws_path(id), &json!({ "packages": packages }))
}

pub fn add(api: &Api, id: &str, entries: &[String]) -> Result<(), String> {
    let doc = api.get(&ws_path(id))?;
    let doc = patch(api, id, merge(&packages(&doc), entries))?;
    for e in entries {
        line(&doc, e, "added ");
    }
    println!("{REBUILD}");
    Ok(())
}

pub fn rm(api: &Api, id: &str, entries: &[String]) -> Result<(), String> {
    let doc = api.get(&ws_path(id))?;
    patch(api, id, remove(&packages(&doc), entries)?)?;
    for e in entries {
        println!("removed {}", attr(e));
    }
    println!("{REBUILD}");
    Ok(())
}

pub fn update(api: &Api, id: &str) -> Result<(), String> {
    let doc = api.post(&format!("{}/packages/update", ws_path(id)))?;
    for e in packages(&doc) {
        line(&doc, &e, "");
    }
    println!("{REBUILD}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn merge_appends_and_repins() {
        assert_eq!(merge(&v(&["jq", "nodejs@20"]), &v(&["cowsay", "nodejs@22"])), v(&["jq", "nodejs@22", "cowsay"]));
        // A duplicate argument collapses onto one entry rather than being declared twice.
        assert_eq!(merge(&v(&[]), &v(&["jq", "jq@1.7"])), v(&["jq@1.7"]));
    }

    #[test]
    fn remove_is_by_attr_and_refuses_what_is_not_there() {
        assert_eq!(remove(&v(&["jq", "nodejs@20"]), &v(&["nodejs@99"])).unwrap(), v(&["jq"]));
        assert_eq!(remove(&v(&["jq"]), &v(&["cowsay"])).unwrap_err(), "cowsay: not in the package list");
    }

    #[test]
    fn locked_version_comes_from_the_entry_it_names() {
        let doc = json!({"packages": ["nodejs@20"], "locks": [{"entry": "nodejs@20", "version": "20.11.1"}]});
        assert_eq!(locked(&doc, "nodejs@20").as_deref(), Some("20.11.1"));
        assert_eq!(locked(&doc, "jq"), None);
    }
}
