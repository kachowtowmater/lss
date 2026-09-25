//! Card #36: watching omp's shared config (`~/.omp/agent/config.yml`) for a stale default model.
//! 2026-09-20: it named a model this provider no longer served, and omp fell back to a model at
//! an OUTSIDE provider - without a word. Agent work silently left this hardware. This module only
//! ever WATCHES; it never writes. Re-pointing the file is a separate job, and should be done the
//! way any config rewrite should be (atomically, under a lock, with a backup) - kept out of here
//! on purpose, the same way the rule engine only ever OBSERVES the serve and never restarts it.
//!
//! No YAML dependency: `modelRoles: / default: <id>` is one targeted, well-known shape - read
//! exactly that, not a general YAML document.

/// The value after `modelRoles: / default:` - the usual `provider/model-id` form
/// (e.g. `acme/model-a`). `None` if the file has no `modelRoles:` block in the
/// expected shape - never guessed, never invented.
pub fn parse_default_model(yaml_text: &str) -> Option<String> {
    let mut in_roles = false;
    for line in yaml_text.lines() {
        if line.starts_with("modelRoles:") {
            in_roles = true;
            continue;
        }
        if in_roles {
            if let Some(rest) = line.strip_prefix("  default:") {
                let v = rest.trim();
                return (!v.is_empty()).then(|| v.to_string());
            }
            // a non-indented, non-blank line means the modelRoles block ended without a default
            if !line.starts_with(' ') && !line.trim().is_empty() {
                return None;
            }
        }
    }
    None
}

/// `configured` = `modelRoles.default`, whatever provider prefix it carries; `served` = what
/// the collector's own poll says is actually running right now (no provider prefix - the
/// collector never assumes one). They agree when `configured`'s suffix after the LAST `/`
/// equals `served` exactly. `None` when either side is unknown (nothing to compare yet, not a
/// mismatch) - `Some(configured, served)` only when there is a genuine, provable disagreement.
pub fn mismatch(configured: Option<&str>, served: Option<&str>) -> Option<(String, String)> {
    let c = configured?;
    let s = served?;
    let c_suffix = c.rsplit('/').next().unwrap_or(c);
    (c_suffix != s).then(|| (c.to_string(), s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "\
defaultProvider: acme
enabledModels:
  - acme/*
tools:
  approvalMode: yolo
modelRoles:
  default: acme/model-a
browser:
  relay: true
";

    #[test]
    fn extracts_the_default_out_of_a_real_shaped_config() {
        assert_eq!(parse_default_model(CONFIG).as_deref(), Some("acme/model-a"));
    }

    #[test]
    fn no_modelroles_block_at_all_is_none_not_a_guess() {
        assert_eq!(parse_default_model("defaultProvider: acme\ntools:\n  approvalMode: yolo\n"), None);
    }

    #[test]
    fn a_modelroles_block_with_no_default_line_is_none() {
        assert_eq!(parse_default_model("modelRoles:\n  smol: acme/small\nbrowser:\n  relay: true\n"), None);
    }

    #[test]
    fn an_empty_default_value_is_none_not_an_empty_string() {
        assert_eq!(parse_default_model("modelRoles:\n  default: \nbrowser:\n  relay: true\n"), None);
    }

    #[test]
    fn modelroles_as_the_very_last_block_in_the_file_still_parses() {
        assert_eq!(parse_default_model("tools:\n  approvalMode: yolo\nmodelRoles:\n  default: acme/x\n"), Some("acme/x".into()));
    }

    #[test]
    fn mismatch_compares_the_suffix_after_the_last_slash_and_ignores_the_provider_prefix() {
        assert_eq!(mismatch(Some("acme/model-a"), Some("model-a")), None, "in sync");
        assert_eq!(mismatch(Some("acme/model-b"), Some("model-a")), Some(("acme/model-b".into(), "model-a".into())));
        assert_eq!(mismatch(None, Some("model-a")), None, "nothing configured yet - not a mismatch, nothing to compare");
        assert_eq!(mismatch(Some("acme/model-a"), None), None, "nothing served yet - not a mismatch, nothing to compare");
    }

    #[test]
    fn a_bare_id_with_no_provider_prefix_still_compares_correctly() {
        // the 2026-09-20 incident's own shape, generalised: whatever is written after the last
        // slash (or the whole value, if there is no slash at all) is what must match
        assert_eq!(mismatch(Some("model-a"), Some("model-a")), None);
        assert_eq!(mismatch(Some("model-b"), Some("model-a")), Some(("model-b".into(), "model-a".into())));
    }
}
