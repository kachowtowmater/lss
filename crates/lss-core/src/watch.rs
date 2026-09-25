//! Card #74: a WATCH page - what the sources an owner actually follows for serving recipes have
//! published, and when. The owner: "if there is a page that shows an update to new serve method
//! from the people we follow that would even be cooler - then ill know okay someone posted an
//! update for the model I'm running, another posted about a different one ... last updated 2
//! hours ago. stuff like this is more useful than seeing advice, incidents and alerts."
//!
//! This collector never reaches the outside network itself to build this - the same "no new
//! source" discipline every other card in this project follows (`rates.rs`: never a new watt
//! reading; `tokens_run.rs`: never a new metric). A SEPARATE process the owner runs on whatever
//! schedule they choose (a cron job, an agent sweep, a plain script - `scripts/watch-check.sh`
//! ships a generic starting point) writes this file; the collector only reads it back and
//! republishes it on `/status`, honestly, the same "[rates] path" pattern card #75 already
//! established (`[watch] path` in `collector.toml`, `""` = off, never in this repo, see
//! `packaging/watch.json.example`).
//!
//! HARD CONSTRAINTS the card itself states, each bought with a real mistake:
//!  - a source's `newest` item is its own CONTENT (a real summary of the release body), never
//!    its title alone (2026-09-01: a title-only sweep reported a defect the card's own text said
//!    was someone else's problem).
//!  - `has_receipt: false` (a bare social post, no issue/PR/commit/file:line/HF id) MUST read
//!    UNVERIFIED on screen, never presented as a finding.
//!  - this whole file is read-only information: nothing in this crate or `lss-collector` ever
//!    acts on it (switches an arm, restarts a service) - a human decision, always.

use serde::{Deserialize, Serialize};

/// One item a source published - a release, a post, a commit. `date` is the item's OWN date
/// ("YYYY-MM-DD", never guessed at) so "last updated 2 hours ago" phrasing on screen is worked
/// out from `WatchSource::last_checked` (when the CHECK ran), not from this - the two are
/// different facts and a screen must never blur them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchItem {
    pub date: String,
    /// the CONTENT, never a bare title - see this module's own doc comment
    pub summary: String,
    pub url: String,
    /// `false` = a bare social/hypothesis-tier claim with no receipt (issue, PR, commit,
    /// file:line, HF id) - MUST render UNVERIFIED, never as a finding
    pub has_receipt: bool,
}

/// One followed source: what it is, what it is followed FOR, and the newest thing it published.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchSource {
    pub name: String,
    /// plain English: what this source's recipes actually feed (e.g. "GLM loadouts on the GPU
    /// box") - the DONE criterion is explicit that this must be on screen, not just a bare name
    pub covers: String,
    /// unix ts of the last time THIS source was actually checked - `None` = never checked yet.
    /// Its own field, not a whole-file timestamp: one file can hold sources checked on different
    /// cadences (a fast-moving repo hourly, a slow one daily), and "last checked Nh ago" must be
    /// honest per source, not borrowed from whichever one happened to run most recently.
    pub last_checked: Option<i64>,
    /// `None` = checked, and it published nothing this check has ever found - distinct from
    /// "never checked" (`last_checked: None`); a screen must say which of the two is true, never
    /// blur them into the same blank.
    pub newest: Option<WatchItem>,
}

/// The whole file `[watch] path` points at.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchStatus {
    pub sources: Vec<WatchSource>,
}

/// `text` is exactly the file's own JSON. A malformed file is an error message naming what
/// broke, never a panic and never a half-parsed document - `main.rs` treats it the same as a
/// missing file: watch tracking is off until it is fixed, logged once, never fatal.
pub fn parse(text: &str) -> Result<WatchStatus, String> {
    serde_json::from_str(text).map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_shaped_file() {
        let text = r#"{
            "sources": [
                {"name": "example-recipes", "covers": "model loadouts on the GPU box", "last_checked": 1790000000,
                 "newest": {"date": "2026-09-20", "summary": "swapped the chat template for a fixed one from upstream, gated PASS same day", "url": "https://example.com/r/1", "has_receipt": true}},
                {"name": "a social account", "covers": "reference only", "last_checked": 1790000000,
                 "newest": {"date": "2026-09-19", "summary": "claims a new kernel is 2x faster, no link", "url": "", "has_receipt": false}},
                {"name": "never checked yet", "covers": "reference only", "last_checked": null, "newest": null}
            ]
        }"#;
        let w = parse(text).unwrap();
        assert_eq!(w.sources.len(), 3);
        assert!(w.sources[0].newest.as_ref().unwrap().has_receipt);
        assert!(!w.sources[1].newest.as_ref().unwrap().has_receipt, "a bare social claim is never has_receipt: true");
        assert!(w.sources[2].last_checked.is_none() && w.sources[2].newest.is_none());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse("{not json").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn an_empty_sources_list_parses_fine() {
        assert_eq!(parse(r#"{"sources": []}"#).unwrap().sources.len(), 0);
    }
}
