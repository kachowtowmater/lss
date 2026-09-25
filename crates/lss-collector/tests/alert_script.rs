//! Drives the REAL scripts/lss-alert.sh with a fake `ssh`, `agentctl` (the agent CLI the
//! script drives via LSS_ALERT_AGENT_CMD) and `osascript` on PATH.
//! The fake ssh runs the remote command locally, so the script's own quoting, base64 hop and
//! `<cmd> agent wait && <cmd> agent prompt` line are what gets exercised.
//!
//! Fake seat state lives in files under $FAKE:
//!   agent_state   "idle" | "busy"      what `<cmd> agent wait` answers
//!   idle_budget   N (optional)         the agent turns busy after N prompts (it starts working)
//!   ssh_down      (exists)             ssh itself fails with 255
//!   delivered.log                      one line per `<cmd> agent prompt` text
//!   banners.log                        one line per osascript banner

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

// card #336: executable test files are written by a child process (Text file busy)
#[path = "common/exec_file.rs"]
mod exec_file;

const FAKE_SSH: &str = r#"#!/bin/bash
while [ "$1" = "-o" ]; do shift 2; done
echo "$1" >>"$FAKE/ssh-hosts.log"
shift
[ -e "$FAKE/ssh_down" ] && { echo "ssh: connect to host seat-box port 22: Connection timed out" >&2; exit 255; }
exec bash -c "$*"
"#;

const FAKE_AGENTCTL: &str = r#"#!/bin/bash
# agentctl agent wait <target> ... | agentctl agent prompt <target> <text>
state=$(cat "$FAKE/agent_state" 2>/dev/null || echo idle)
# card #249: a target named in $FAKE/blocked exists but sits in a permission dialog
if grep -qxF -- "$3" "$FAKE/blocked" 2>/dev/null; then
  [ "$2" = wait ] && echo "$3" >>"$FAKE/wait-targets.log"
  echo "{\"error\":{\"code\":\"agent_blocked\",\"message\":\"agent $3 is blocked\"},\"id\":\"cli:agent:$2\"}" >&2
  exit 1
fi
# card #245: a target named in $FAKE/missing does not exist on the seat
if grep -qxF -- "$3" "$FAKE/missing" 2>/dev/null; then
  [ "$2" = wait ] && echo "$3" >>"$FAKE/wait-targets.log"
  echo "{\"error\":{\"code\":\"agent_not_found\",\"message\":\"agent target $3 not found\"},\"id\":\"cli:agent:$2\"}" >&2
  exit 1
fi
if [ -e "$FAKE/idle_budget" ] && [ "$(cat "$FAKE/idle_budget")" -le 0 ]; then state=busy; fi
case "$2" in
  wait)
    echo "$3" >>"$FAKE/wait-targets.log"
    if [ "$state" = "busy" ]; then
      echo '{"error":{"code":"timeout","message":"timed out waiting for agent status"},"id":"cli:agent:wait"}' >&2
      exit 1
    fi ;;
  prompt)
    printf '%s\n' "$4" >>"$FAKE/delivered.log"
    [ -e "$FAKE/idle_budget" ] && echo $(( $(cat "$FAKE/idle_budget") - 1 )) >"$FAKE/idle_budget"
    echo '{"id":"cli:agent:prompt","result":{"type":"ok"}}' ;;
esac
exit 0
"#;

const FAKE_OSASCRIPT: &str = "#!/bin/bash\nprintf '%s\\n' \"${!#}\" >>\"$FAKE/banners.log\"\n";

static N: AtomicU32 = AtomicU32::new(0);

struct Seat {
    dir: PathBuf,
}

struct Run {
    stdout: String,
}

impl Seat {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("lss-alert-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        for (name, body) in [("ssh", FAKE_SSH), ("agentctl", FAKE_AGENTCTL), ("osascript", FAKE_OSASCRIPT)] {
            let p = dir.join("bin").join(name);
            exec_file::write_exec(&p, body);
        }
        let seat = Self { dir };
        seat.agent("idle");
        seat
    }

    fn agent(&self, state: &str) {
        std::fs::write(self.dir.join("agent_state"), state).unwrap();
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/lss-alert.sh");
        let path = format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        let mut cmd = Command::new("bash");
        cmd.arg(script).args(args).env("PATH", path).env("FAKE", &self.dir).env("HOME", &self.dir).env("LSS_STATE_DIR", self.dir.join("state"));
        for k in ["LSS_ALERT_TEST", "LSS_ALERT_DRY_RUN", "LSS_ALERT_ID", "LSS_ALERT_SPOOL_MAX", "LSS_ALERT_DIGEST_OVER", "LSS_ALERT_ENV", "LSS_ALERT_AGENT_FALLBACK", "LSS_ALERT_AGENT_CMD"] {
            cmd.env_remove(k);
        }
        // what a site's ~/.config/lss/alert.env would set (the script itself names nobody)
        cmd.env("LSS_ALERT_SEAT", "seat-box").env("LSS_ALERT_AGENT", "agent-main").env("LSS_ALERT_TAG", "[FROM-lss-gpu-box]");
        let out = cmd.envs(env.iter().copied()).output().expect("bash runs");
        assert!(out.status.success(), "the alert script must never fail its caller: {out:?}");
        Run { stdout: String::from_utf8_lossy(&out.stdout).into_owned() }
    }

    fn run(&self, args: &[&str]) -> Run {
        self.run_env(args, &[])
    }

    fn lines(&self, file: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.join(file)).unwrap_or_default().lines().map(String::from).collect()
    }

    fn spool(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.join("state/mail-spool"))
            .map(|d| d.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".msg")).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("state/alert.log")).unwrap_or_default()
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn idle_agent_gets_the_mail_at_once_tagged_as_agent_mail() {
    let seat = Seat::new();
    let r = seat.run(&["info", "rejected_429 grew by 31"]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=OK banner=SKIP delivered=1 spool=0");
    assert_eq!(seat.lines("delivered.log"), vec!["[FROM-lss-gpu-box] [info] rejected_429 grew by 31"]);
    assert_eq!(seat.lines("ssh-hosts.log"), vec!["seat-box"]);
    assert_eq!(seat.lines("wait-targets.log"), vec!["agent-main"], "the agent is only prompted after a wait for idle/done");
    assert!(seat.spool().is_empty());
}

#[test]
fn busy_then_spooled_then_idle_then_flushed() {
    let seat = Seat::new();
    seat.agent("busy");
    let r = seat.run_env(&["info", "spool me"], &[("LSS_ALERT_ID", "41")]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=SKIP delivered=0 spool=1", "an info alert to a busy agent is queued, not lost");
    assert_eq!(seat.spool().len(), 1);
    assert!(seat.lines("delivered.log").is_empty());
    assert!(seat.log().contains("status=SPOOLED channel=agent-mail target=agent-main reason=agent_busy"), "{}", seat.log());

    // still busy: a flush changes nothing and says so
    assert_eq!(seat.run(&["--flush"]).stdout.trim(), "lss-alert: flush flushed=0 spool=1");
    assert!(seat.log().contains("detail=flush_failed reason=agent_busy"));
    assert_eq!(seat.spool().len(), 1);

    seat.agent("idle");
    assert_eq!(seat.run(&["--flush"]).stdout.trim(), "lss-alert: flush flushed=1 spool=0");
    let got = seat.lines("delivered.log");
    assert_eq!(got.len(), 1);
    assert!(got[0].starts_with("[FROM-lss-gpu-box] [info] spool me (queued "), "{got:?}");
    assert!(got[0].ends_with(", delivered late)"), "{got:?}");
    assert!(seat.spool().is_empty());
    assert!(seat.log().contains("status=OK channel=agent-mail target=agent-main detail=flushed file="));
    assert_eq!(seat.lines("state/mail-delivered.ids"), vec!["41"], "the collector is told which alert rows got through late");
    // nothing left: a flush is a no-op that touches no network
    let hops = seat.lines("ssh-hosts.log").len();
    assert_eq!(seat.run(&["--flush"]).stdout.trim(), "lss-alert: flush flushed=0 spool=0");
    assert_eq!(seat.lines("ssh-hosts.log").len(), hops);
}

#[test]
fn every_invocation_flushes_the_spool_first_oldest_first() {
    let seat = Seat::new();
    seat.agent("busy");
    seat.run(&["info", "first"]);
    seat.run(&["info", "second"]);
    assert_eq!(seat.spool().len(), 2);
    seat.agent("idle");
    let r = seat.run(&["info", "third"]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=OK banner=SKIP delivered=1 spool=0");
    let got = seat.lines("delivered.log");
    assert_eq!(got.len(), 3);
    assert!(got[0].contains("] first (queued") && got[1].contains("] second (queued"), "{got:?}");
    assert_eq!(got[2], "[FROM-lss-gpu-box] [info] third");
}

#[test]
fn flush_stops_at_the_first_failure_and_new_mail_queues_behind_it() {
    let seat = Seat::new();
    seat.agent("busy");
    for m in ["a", "b", "c"] {
        seat.run(&["info", m]);
    }
    seat.agent("idle");
    std::fs::write(seat.dir.join("idle_budget"), "1").unwrap(); // the agent starts working after one prompt
    let r = seat.run(&["info", "d"]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=SKIP delivered=0 spool=3");
    let got = seat.lines("delivered.log");
    assert_eq!(got.len(), 1, "one delivered, then the agent was busy again: {got:?}");
    assert!(got[0].contains("] a (queued"));
    // order is kept: b, c, then d
    let texts: Vec<String> = seat.spool().iter().map(|f| std::fs::read_to_string(seat.dir.join("state/mail-spool").join(f)).unwrap()).collect();
    assert!(texts[0].contains("] b") && texts[1].contains("] c") && texts[2].contains("] d"), "{texts:?}");
    assert!(seat.log().contains("reason=queued_behind_spool:agent_busy"));
}

#[test]
fn more_than_five_spooled_messages_arrive_as_one_digest() {
    let seat = Seat::new();
    seat.agent("busy");
    for i in 1..=7 {
        seat.run_env(&["info", &format!("alert number {i}")], &[("LSS_ALERT_ID", &format!("{}", 100 + i))]);
    }
    assert_eq!(seat.spool().len(), 7);
    seat.agent("idle");
    assert_eq!(seat.run(&["--flush"]).stdout.trim(), "lss-alert: flush flushed=7 spool=0");
    let got = seat.lines("delivered.log");
    assert_eq!(got.len(), 1, "ONE message, not seven: {got:?}");
    assert!(got[0].starts_with("[FROM-lss-gpu-box] [digest] 7 alerts were queued while agent-main was not idle (oldest first):"), "{got:?}");
    for i in 1..=7 {
        assert!(got[0].contains(&format!("({i}) ")) && got[0].contains(&format!("[info] alert number {i}")), "entry {i} missing: {got:?}");
    }
    assert!(got[0].find("alert number 1").unwrap() < got[0].find("alert number 7").unwrap());
    assert_eq!(got[0].matches("[FROM-lss-gpu-box]").count(), 1, "the tag leads the message once");
    assert!(seat.spool().is_empty());
    assert_eq!(seat.lines("state/mail-delivered.ids").len(), 7);
    assert!(seat.log().contains("detail=flushed_digest count=7"));
}

#[test]
fn five_or_fewer_are_delivered_one_by_one() {
    let seat = Seat::new();
    seat.agent("busy");
    for i in 1..=5 {
        seat.run(&["info", &format!("m{i}")]);
    }
    seat.agent("idle");
    assert_eq!(seat.run(&["--flush"]).stdout.trim(), "lss-alert: flush flushed=5 spool=0");
    assert_eq!(seat.lines("delivered.log").len(), 5);
}

#[test]
fn the_spool_is_capped_and_drops_the_oldest_with_a_log_line() {
    let seat = Seat::new();
    seat.agent("busy");
    for i in 1..=5 {
        seat.run_env(&["info", &format!("cap {i}")], &[("LSS_ALERT_SPOOL_MAX", "3")]);
    }
    let files = seat.spool();
    assert_eq!(files.len(), 3);
    let oldest = std::fs::read_to_string(seat.dir.join("state/mail-spool").join(&files[0])).unwrap();
    assert!(oldest.contains("cap 3"), "1 and 2 were dropped: {oldest}");
    let log = seat.log();
    assert_eq!(log.matches("status=DROP channel=agent-mail detail=spool_full max=3").count(), 2, "{log}");
    assert!(log.contains("text=[FROM-lss-gpu-box] [info] cap 1"), "a dropped message is still readable in the log");
}

#[test]
fn ssh_failure_spools_too_and_a_warn_falls_back_to_a_banner() {
    let seat = Seat::new();
    std::fs::write(seat.dir.join("ssh_down"), "").unwrap();
    let r = seat.run(&["warn", "serve DOWN for 2m05s"]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=FAIL delivered=0 spool=1");
    assert!(seat.log().contains("reason=ssh_or_agentmail_failed"));
    std::fs::remove_file(seat.dir.join("ssh_down")).unwrap();
    seat.agent("busy");
    let r = seat.run(&["warn", "gate DOWN for 2m00s"]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=OK delivered=1 spool=2", "the warn reached a human NOW and the mail is still owed");
    assert_eq!(seat.lines("banners.log"), vec!["[warn] gate DOWN for 2m00s"]);
}

#[test]
fn page_and_hardware_always_banner() {
    let seat = Seat::new();
    let r = seat.run(&["hardware", "GPU3 Xid 79"]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=OK banner=OK delivered=1 spool=0");
    assert_eq!(seat.lines("banners.log"), vec!["[hardware] GPU3 Xid 79"]);
    assert_eq!(seat.run(&["info", "quiet"]).stdout.trim(), "lss-alert: mail=OK banner=SKIP delivered=1 spool=0");
    assert_eq!(seat.lines("banners.log").len(), 1, "info never banners");
}

#[test]
fn quoting_survives_the_ssh_hop_and_newlines_are_flattened() {
    let seat = Seat::new();
    let nasty = "it's \"quoted\" $HOME `id` ; rm -rf / && echo\nsecond line";
    seat.run(&["info", nasty]);
    assert_eq!(seat.lines("delivered.log"), vec!["[FROM-lss-gpu-box] [info] it's \"quoted\" $HOME `id` ; rm -rf / && echo second line"]);
}

#[test]
fn test_prefix_dedupe_dry_run_and_bad_input() {
    let seat = Seat::new();
    seat.run_env(&["info", "install test"], &[("LSS_ALERT_TEST", "1")]);
    assert_eq!(seat.lines("delivered.log"), vec!["[FROM-lss-gpu-box] [info] [TEST] install test"]);
    // the same alert again inside 60 s is dropped
    let r = seat.run_env(&["info", "install test"], &[("LSS_ALERT_TEST", "1")]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SKIP banner=SKIP delivered=0 spool=0");
    assert_eq!(seat.lines("delivered.log").len(), 1);
    // dry run: nothing sent, nothing spooled, even with a busy agent
    seat.agent("busy");
    let r = seat.run_env(&["page", "dry"], &[("LSS_ALERT_DRY_RUN", "1")]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=OK banner=OK delivered=1 spool=0");
    assert_eq!(seat.lines("delivered.log").len(), 1);
    assert!(seat.lines("banners.log").is_empty());
    assert!(seat.spool().is_empty());
    // empty message and unknown severity
    assert_eq!(seat.run(&["info", ""]).stdout.trim(), "lss-alert: mail=SKIP banner=SKIP delivered=0 spool=0");
    seat.agent("idle");
    seat.run(&["bogus", "odd severity"]);
    assert!(seat.lines("delivered.log").last().unwrap().contains("[warn] odd severity"));
    // retargeting
    seat.run_env(&["info", "elsewhere"], &[("LSS_ALERT_AGENT", "agent-other"), ("LSS_ALERT_SEAT", "other-seat")]);
    assert_eq!(seat.lines("ssh-hosts.log").last().unwrap(), "other-seat");
    assert_eq!(seat.lines("wait-targets.log").last().unwrap(), "agent-other");
}

// card #245: our site mailed a target that no longer existed for four days (575 FAIL lines),
// and after a public-default change it mailed through a CLI the seat did not have, and both read
// as "ssh_or_agentmail_failed". A fallback target covers a renamed/restarted agent; a missing CLI
// is named as exactly that.

#[test]
fn a_target_that_does_not_exist_falls_back_to_the_next_one() {
    let seat = Seat::new();
    std::fs::write(seat.dir.join("missing"), "agent-main\nagent-gone\n").unwrap();
    let r = seat.run_env(&["warn", "serve down 2m"], &[("LSS_ALERT_AGENT_FALLBACK", "agent-gone agent-pane")]);
    let r2 = seat.run_env(&["info", "second"], &[("LSS_ALERT_AGENT_FALLBACK", "agent-gone agent-pane")]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=OK banner=SKIP delivered=1 spool=0", "{}", seat.log());
    assert_eq!(r2.stdout.trim(), "lss-alert: mail=OK banner=SKIP delivered=1 spool=0", "{}", seat.log());
    assert_eq!(seat.lines("delivered.log"), vec!["[FROM-lss-gpu-box] [warn] serve down 2m", "[FROM-lss-gpu-box] [info] second"]);
    assert_eq!(seat.lines("wait-targets.log"), vec!["agent-main", "agent-gone", "agent-pane", "agent-main", "agent-gone", "agent-pane"], "tried in order, each only after the one before is not found");
    let log = seat.log();
    assert!(log.contains("status=OK channel=agent-mail target=agent-pane") && log.contains("detail=delivered_to_fallback target=agent-pane primary=agent-main"), "{log}");
}

#[test]
fn a_busy_target_is_waited_for_never_redirected_to_the_fallback() {
    let seat = Seat::new();
    seat.agent("busy");
    let r = seat.run_env(&["info", "queue grew"], &[("LSS_ALERT_AGENT_FALLBACK", "agent-pane")]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=SKIP delivered=0 spool=1");
    assert_eq!(seat.lines("wait-targets.log"), vec!["agent-main"], "a busy agent exists: the mail waits for it in the spool");
    assert!(seat.lines("delivered.log").is_empty());
}

#[test]
fn every_target_missing_spools_with_agent_not_found() {
    let seat = Seat::new();
    std::fs::write(seat.dir.join("missing"), "agent-main\nagent-pane\n").unwrap();
    let r = seat.run_env(&["info", "x"], &[("LSS_ALERT_AGENT_FALLBACK", "agent-pane")]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=SKIP delivered=0 spool=1");
    assert!(seat.log().contains("reason=agent_not_found"), "{}", seat.log());
}

#[test]
fn an_agent_cli_the_seat_does_not_have_is_named_as_that() {
    let seat = Seat::new();
    let r = seat.run_env(&["warn", "x"], &[("LSS_ALERT_AGENT_CMD", "no-such-agent-cli")]);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=OK delivered=1 spool=1", "a warn still reaches the banner");
    let log = seat.log();
    assert!(log.contains("reason=agent_cmd_not_found") && log.contains("hint=set_LSS_ALERT_AGENT_CMD_in_") && !log.contains("reason=ssh_or_agentmail_failed"), "{log}");
}

// card #249 (lss-verifier-3 on #245): a BLOCKED agent exists - it is waiting on a dialog - so its
// mail waits in the spool for it and is never redirected to the fallback; once the dialog is
// answered the spool flushes to the SAME agent.
#[test]
fn a_blocked_target_is_spooled_and_retried_never_redirected_to_the_fallback() {
    let seat = Seat::new();
    std::fs::write(seat.dir.join("blocked"), "agent-main\n").unwrap();
    let fb = [("LSS_ALERT_AGENT_FALLBACK", "agent-pane")];
    let r = seat.run_env(&["info", "queue grew"], &fb);
    assert_eq!(r.stdout.trim(), "lss-alert: mail=SPOOLED banner=SKIP delivered=0 spool=1", "{}", seat.log());
    assert_eq!(seat.lines("wait-targets.log"), vec!["agent-main"], "blocked is not missing: no fallback is tried");
    assert!(seat.lines("delivered.log").is_empty());
    assert!(seat.log().contains("reason=agent_blocked") && !seat.log().contains("delivered_to_fallback"), "{}", seat.log());
    // the dialog is answered: the retry goes to the same agent
    std::fs::remove_file(seat.dir.join("blocked")).unwrap();
    let r = seat.run_env(&["--flush"], &fb);
    assert_eq!(r.stdout.trim(), "lss-alert: flush flushed=1 spool=0", "{}", seat.log());
    assert_eq!(seat.lines("wait-targets.log"), vec!["agent-main", "agent-main"]);
    assert_eq!(seat.lines("delivered.log").len(), 1);
    assert!(seat.log().contains("status=OK channel=agent-mail target=agent-main detail=flushed"), "{}", seat.log());
}
