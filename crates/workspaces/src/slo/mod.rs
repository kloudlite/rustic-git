//! The SLO probe's catalogue lives here, separate from `history::alerts`, because it is judged by
//! a synthetic user's own runs (`kloudlite.slo_runs`/`slo_results`) rather than the collector's
//! metric tables — the two catalogues share the "one Rust source, one held-equal markdown file"
//! shape but answer different questions (a symptom vs. a completed journey step).

pub mod catalogue;
