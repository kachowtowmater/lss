//! `docker inspect` output parsing.

use crate::timeutil::parse_rfc3339;
use serde::{Deserialize, Serialize};

/// One line per container, pipe-separated - pipes cannot appear in any of these fields.
pub const INSPECT_FORMAT: &str = "{{.Name}}|{{.State.Status}}|{{.RestartCount}}|{{.State.StartedAt}}";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerState {
    pub name: String,
    /// docker's state string: running, restarting, exited, created, paused, dead.
    pub status: String,
    pub restart_count: u64,
    /// unix seconds; 0 when docker reports the zero time (never started).
    pub started_at: i64,
}

impl ContainerState {
    pub fn running(&self) -> bool {
        self.status == "running"
    }
}

pub fn parse_inspect(text: &str) -> Vec<ContainerState> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.trim().split('|').collect();
            if f.len() != 4 {
                return None;
            }
            Some(ContainerState {
                name: f[0].trim_start_matches('/').to_string(),
                status: f[1].to_string(),
                restart_count: f[2].parse().ok()?,
                started_at: parse_rfc3339(f[3]).unwrap_or(0).max(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_inspect_lines() {
        let text = "/the gateway|running|0|2026-09-19T11:41:10.66815265Z\n\
                    /model-a-sglang-sm120|running|2|2026-09-19T11:36:29.229739584Z\n\
                    Error: No such object: nope\n\
                    /never|created|0|0001-01-01T00:00:00Z\n";
        let c = parse_inspect(text);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].name, "the gateway");
        assert!(c[0].running());
        assert_eq!(c[1].restart_count, 2);
        assert_eq!(c[1].started_at, 1_789_817_789);
        assert_eq!(c[2].started_at, 0);
        assert!(!c[2].running());
    }
}
