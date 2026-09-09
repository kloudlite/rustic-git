use crate::api;
use crate::config;

pub async fn status(team: Option<&str>) -> Result<(), String> {
    let cfg = config::load()?;
    let b = api::builder_status(&cfg, team).await.map_err(|e| e.to_string())?;
    println!("{}", render(&b));
    Ok(())
}

/// The three lines, factored out so it can be checked from a body with no network — `why` is
/// the first condition that is not True, since that is the one thing blocking readiness; "-"
/// when every condition is True (or there are none, before the controller has written status).
fn render(b: &api::BuilderStatus) -> String {
    let why = b
        .conditions
        .iter()
        .find(|c| c.status != "True")
        .map(|c| c.message.as_str())
        .unwrap_or("-");
    format!("state: {}\nready: {}\nwhy: {why}", b.state, b.ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{BuilderStatus, Condition};

    #[test]
    fn ready_with_no_bad_condition_says_dash() {
        let b = BuilderStatus {
            state: "running".into(),
            ready: true,
            conditions: vec![Condition { type_: "Ready".into(), status: "True".into(), message: "buildkit is up".into() }],
        };
        assert_eq!(render(&b), "state: running\nready: true\nwhy: -");
    }

    #[test]
    fn waiting_names_the_first_non_true_condition() {
        let b = BuilderStatus {
            state: "creating".into(),
            ready: false,
            conditions: vec![
                Condition { type_: "Placed".into(), status: "False".into(), message: "no capacity".into() },
                Condition { type_: "Ready".into(), status: "False".into(), message: "buildkit is not up".into() },
            ],
        };
        assert_eq!(render(&b), "state: creating\nready: false\nwhy: no capacity");
    }

    /// The wire shape, not a struct literal: `type` is a Rust keyword and `conditions` is absent
    /// before the controller has written status, so both are places the rendering can break
    /// against a real body while every literal-built test still passes.
    #[test]
    fn a_real_body_deserialises_and_renders() {
        let b: BuilderStatus = serde_json::from_str(
            r#"{"id":"bld-acme","state":"creating","ready":false,
                "conditions":[{"type":"Ready","status":"False","message":"buildkit is not up"}]}"#,
        )
        .unwrap();
        assert_eq!(render(&b), "state: creating\nready: false\nwhy: buildkit is not up");

        let b: BuilderStatus =
            serde_json::from_str(r#"{"id":"bld-acme","state":"creating","ready":false}"#).unwrap();
        assert_eq!(render(&b), "state: creating\nready: false\nwhy: -");
    }
}
