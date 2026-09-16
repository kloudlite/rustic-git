//! `kl env` — which of the team's environments this person's space follows. The choice is the
//! person's (`/v1/me/environments`), not the workspace's; the agent converges every pod of the
//! space from it, so switching here moves every workspace the person has in this team.

use serde_json::{json, Value};

use crate::api::Api;

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(|a| a.as_slice()).unwrap_or_default()
}

fn field<'a>(e: &'a Value, k: &str) -> &'a str {
    e.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// The environment this team's space follows, from `GET /v1/me/environments`.
fn current_id(me: &Value, team: &str) -> Option<String> {
    arr(me).iter().find(|s| field(s, "team").eq_ignore_ascii_case(team)).map(|s| field(s, "environment").to_string()).filter(|s| !s.is_empty())
}

/// An id passes through; a name must match exactly one environment, and two matches are refused
/// naming both ids rather than picking one.
pub fn resolve(envs: &[Value], target: &str) -> Result<String, String> {
    if envs.iter().any(|e| field(e, "id") == target) {
        return Ok(target.to_string());
    }
    let hits: Vec<&Value> = envs.iter().filter(|e| field(e, "name").eq_ignore_ascii_case(target)).collect();
    match hits.as_slice() {
        [one] => Ok(field(one, "id").to_string()),
        [] => Err(format!("{target}: no such environment in this team — `kl env list`")),
        many => Err(format!("{target} names {} environments ({}); use the id", many.len(), many.iter().map(|e| field(e, "id")).collect::<Vec<_>>().join(", "))),
    }
}

fn list_envs(api: &Api, team: &str) -> Result<Value, String> {
    api.get(&format!("/v1/environments?owner={team}"))
}

pub fn list(api: &Api, team: &str) -> Result<(), String> {
    let envs = list_envs(api, team)?;
    let cur = current_id(&api.get("/v1/me/environments")?, team);
    if arr(&envs).is_empty() {
        println!("no environments in {team}");
        return Ok(());
    }
    for e in arr(&envs) {
        let mark = if Some(field(e, "id").to_string()) == cur { "*" } else { " " };
        println!("{mark} {} ({})", field(e, "name"), field(e, "id"));
    }
    Ok(())
}

pub fn current(api: &Api, team: &str) -> Result<(), String> {
    match current_id(&api.get("/v1/me/environments")?, team) {
        Some(id) => println!("{id}"),
        None => println!("none"),
    }
    Ok(())
}

pub fn switch(api: &Api, team: &str, target: &str) -> Result<(), String> {
    let id = resolve(arr(&list_envs(api, team)?), target)?;
    api.put_json(&format!("/v1/me/environments/{team}"), &json!({ "environment": id }))?;
    println!("switched to {id}");
    Ok(())
}

pub fn clear(api: &Api, team: &str) -> Result<(), String> {
    api.delete(&format!("/v1/me/environments/{team}"))?;
    println!("cleared — this space follows no environment");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envs() -> Vec<Value> {
        vec![json!({"id": "e1", "name": "dev"}), json!({"id": "e2", "name": "dev"}), json!({"id": "e3", "name": "prod"})]
    }

    #[test]
    fn resolution() {
        assert_eq!(resolve(&envs(), "e2").unwrap(), "e2");
        assert_eq!(resolve(&envs(), "prod").unwrap(), "e3");
        assert_eq!(resolve(&envs(), "dev").unwrap_err(), "dev names 2 environments (e1, e2); use the id");
        assert!(resolve(&envs(), "nope").unwrap_err().starts_with("nope: no such environment"));
    }

    #[test]
    fn current_is_this_teams_row_only() {
        let me = json!([{"team": "acme", "environment": "e3"}, {"team": "other", "environment": "e9"}]);
        assert_eq!(current_id(&me, "acme").as_deref(), Some("e3"));
        assert_eq!(current_id(&me, "nobody"), None);
    }
}
