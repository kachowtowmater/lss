//! C1 decode probe: the "may I run now" decision and the stream-timing arithmetic.
//! The HTTP itself lives in the collector.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeDecision {
    Run,
    /// Not time yet.
    NotDue,
    /// A probe is still in flight: never queue a second one.
    InFlight,
    /// Due, but the engine has work: record `skipped_busy`, try again next interval.
    SkipBusy,
    /// Due, but the serve is down: nothing to measure (serve_down covers it).
    SkipDown,
}

pub struct ProbeGate {
    pub now: i64,
    pub last_attempt: Option<i64>,
    pub interval_secs: i64,
    pub in_flight: bool,
    pub serve_up: bool,
    pub running: f64,
    pub queue: f64,
}

pub fn decide(g: &ProbeGate) -> ProbeDecision {
    if g.in_flight {
        return ProbeDecision::InFlight;
    }
    if g.last_attempt.is_some_and(|t| g.now - t < g.interval_secs) {
        return ProbeDecision::NotDue;
    }
    if !g.serve_up {
        return ProbeDecision::SkipDown;
    }
    if g.running > 0.0 || g.queue > 0.0 {
        return ProbeDecision::SkipBusy;
    }
    ProbeDecision::Run
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeRecord {
    pub ts: i64,
    /// ok | skipped_busy | error | timeout
    pub status: String,
    #[serde(default)]
    pub ttft_ms: Option<f64>,
    #[serde(default)]
    pub decode_tok_s: Option<f64>,
    #[serde(default)]
    pub tokens: Option<u32>,
    #[serde(default)]
    pub detail: String,
    /// HTTP status the gate answered with; null when no request was sent (`skipped_busy`) or
    /// no response arrived (connection refused, timeout before the headers)
    #[serde(default)]
    pub http_status: Option<u16>,
    /// Is this an honest IDLE single-stream reading? Only a valid `ok` probe feeds the C1
    /// baseline, the `c1_decode` rule and the `llm_serve_c1_*` metrics. Defaults to true for
    /// documents written before the field existed.
    #[serde(default = "yes")]
    pub valid: bool,
    /// why not: `busy_before` | `slow_ttft` | `contended` | `burst` (or `error` / `timeout`:
    /// no reading at all)
    #[serde(default)]
    pub invalid_reason: Option<String>,
    /// a VALID reading that could not be PROVED to have run alone (no gateway and an engine that
    /// does not report its load): it counts, and it says so (added 2026-09-20)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unverified: bool,
}

fn yes() -> bool {
    true
}

pub const INVALID_BUSY_BEFORE: &str = "busy_before";
pub const INVALID_SLOW_TTFT: &str = "slow_ttft";
pub const INVALID_CONTENDED: &str = "contended";
pub const INVALID_BURST: &str = "burst";

/// The shortest decode window a reading may be measured over, per token: the verifier's
/// "50 ms per 16 tokens". Below it the answer did not stream, it arrived in one piece, and
/// "128 tokens divided by a few milliseconds" is a five-digit number that means nothing.
pub const MIN_MS_PER_TOKEN: f64 = 50.0 / 16.0;
/// Tokens per chunk from which the answer was clearly buffered rather than streamed.
pub const BUFFERED_TOKENS_PER_CHUNK: f64 = 4.0;
/// A reading this many times the learned baseline (or, in the historical repair, the median of
/// the other valid readings) is not this server getting faster. 2x, not 3x (2026-09-20, card #45
/// residual): a genuinely-streamed 445.3 tok/s against a 185.2 baseline (2.40x) was VALID and
/// became the MODEL page's "best" - the burst/chunk test only catches an answer that arrived
/// buffered, and 445.3 streamed in properly-sized chunks, it was just implausibly fast for one
/// real user. 2x keeps real headroom: the next-fastest reading ever recorded on this server,
/// 216.1 (1.17x the baseline), stays comfortably under it.
pub const BURST_OVER_BASELINE: f64 = 2.0;

/// Some(plain reason) = this timing is not a speed measurement but a buffered burst.
/// 2026-09-20: two probes right after a gateway restart were stored as VALID at 26,773 and
/// 5,596 tok/s, and the MODEL page then reported "one user alone 465 tok/s (best 26773.8)".
pub fn burst_reason(t: &ProbeTiming, baseline: Option<f64>) -> Option<String> {
    let per_chunk = f64::from(t.tokens) / f64::from(t.chunks.max(1));
    if per_chunk >= BUFFERED_TOKENS_PER_CHUNK && t.span_ms < f64::from(t.tokens) * MIN_MS_PER_TOKEN {
        return Some(format!(
            "the answer arrived in one burst, not as a stream: {} tokens in {} chunk(s) over {:.0} ms - too short to measure a speed",
            t.tokens, t.chunks, t.span_ms
        ));
    }
    match baseline {
        Some(b) if b > 0.0 && t.decode_tok_s > b * BURST_OVER_BASELINE => Some(format!(
            "{:.0} tok/s is more than {BURST_OVER_BASELINE:.0}x the usual {b:.0} tok/s: the answer was buffered, not streamed",
            t.decode_tok_s
        )),
        _ => None,
    }
}

/// What was seen around one probe. The probe asks "how fast does ONE stream decode on an idle
/// engine"; a request that lands right after the idle check shares the engine with it and the
/// number stops meaning that (seen live: TTFT 57.8 s and 26.5 s, decode 140-150 instead of 190).
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeEvidence {
    /// engine totals read immediately BEFORE sending
    pub running_before: f64,
    pub queue_before: f64,
    pub ttft_ms: f64,
    /// engine totals read immediately AFTER the stream ended (the probe itself may still show as 1)
    pub running_after: Option<f64>,
    pub queue_after: Option<f64>,
    /// gate `admitted`, public + trusted, before and after; None = /gate/health unreadable
    pub admitted_before: Option<u64>,
    pub admitted_after: Option<u64>,
    /// did the gate admit the probe itself (it then accounts for 1 of the growth)
    pub probe_admitted: bool,
    /// is a gateway configured at all? Without one there are no admitted counters to read, and
    /// demanding them would make EVERY probe invalid forever - which is what a new user, who has
    /// no gateway, would get (added 2026-09-20)
    pub gateway: bool,
    /// does the engine report how many requests are running? Ollama, LM Studio and a plain
    /// OpenAI-compatible server do not: nothing can prove the probe ran alone, so it is accepted
    /// with a caveat instead of thrown away
    pub engine_reports_load: bool,
    /// `generation_tokens_total` read at the same instants as running_before/running_after
    /// (added 2026-09-20, card #45): `running` is a gauge sampled every 5 s, so a request that
    /// starts and finishes between two of the poll loop's OWN scrapes is invisible to it - but a
    /// cumulative counter read tightly around the probe's own window cannot miss it, because it
    /// only ever goes up. Live: 3 probes read running=0.0 throughout while the counter advanced
    /// 50-534 tokens every 5 s step across the window (another client's request, mid-flight).
    /// `None` when the adapter has no such counter: the check is skipped, never invented.
    pub gen_tokens_before: Option<f64>,
    pub gen_tokens_after: Option<f64>,
    /// how many tokens the probe's own answer was - what the counter's growth is judged against
    pub probe_tokens: f64,
    /// card #261: the median TTFT of recent VALID probes, i.e. how long this engine takes to
    /// start answering when nobody else is on it. `None` until enough valid probes exist: the
    /// check is skipped, never invented.
    pub ttft_baseline_ms: Option<f64>,
    /// card #266: the engine's `requests_total` and `prompt_tokens_total`, read at the same
    /// instants as `gen_tokens_*` (the "after" read waits until the probe's OWN request has been
    /// counted, see probe_run). Why: on the live box the engine's `running` gauge read 0 the whole time
    /// a 460k-token agent loop was running (2026-09-23 22:14-22:19Z, engine log `#running-req: 1`),
    /// so before/after gauge checks cannot see a foreign request at all - but a request that
    /// finishes moves these counters, and one that does so while the probe is out is proof.
    /// `None` = the adapter has no such counter: the check is skipped, never invented.
    pub requests_before: Option<f64>,
    pub requests_after: Option<f64>,
    pub prompt_tokens_before: Option<f64>,
    pub prompt_tokens_after: Option<f64>,
}

/// card #266: how many prompt tokens the probe's own request may add to `prompt_tokens_total`.
/// The probe prompt is one short sentence; the live foreign request that slipped past on
/// 2026-09-23 22:16Z added 461,756.
pub const PROBE_PROMPT_MARGIN_TOKENS: f64 = 4096.0;

/// card #261: a probe whose TTFT is more than this many times the idle median shared the engine
/// with someone else's prefill, even when every counter read either side of it looked quiet.
/// Live, 2026-09-23 22:16Z: TTFT 259.8 ms against an idle median near 135 ms, decode 143 tok/s
/// against a 185 baseline, and the engine log showed a 200k-token request in flight throughout -
/// yet the probe was stored VALID and became the reading that fired the C1 alert.
pub const C1_TTFT_CONTENDED_RATIO: f64 = 1.5;

/// How many valid probes the idle-TTFT median needs before `ttft_baseline_ms` is trusted.
pub const C1_TTFT_BASELINE_MIN_PROBES: usize = 5;

/// A request the counter proves happened is allowed this many tokens BEYOND the probe's own
/// answer before it counts as contamination: scrape timing means the counter is not always read
/// at the exact instant the stream starts/ends, so a couple of the probe's own tokens can land on
/// either side of the "before" or "after" snapshot. Real interference in the live data was
/// 50-534 tokens per 5 s step - two orders of magnitude past this margin.
pub const COUNTER_MARGIN_TOKENS: f64 = 8.0;

/// What a probe could be shown to be. `CouldNotRuleOut` is still a reading - the median of many
/// of them is what MODEL shows - but it says plainly what was not proved.
#[derive(Debug, Clone, PartialEq)]
pub enum Confidence {
    RanAlone,
    CouldNotRuleOut(&'static str),
}

/// The caveat shown next to a reading that could not be proved to have run alone.
pub const UNVERIFIED_NOTE: &str = "could not rule out other traffic";

/// Ok = a reading. Err = (reason, human detail).
///
/// A reading needs: (a) running == 0 and queue == 0 immediately before; (b) TTFT <= `max_ttft_s`;
/// (c) `generation_tokens_total` grew by no more than the probe's own answer, when the adapter
/// has such a counter - this is the check that decides: `running` is a gauge that misses a
/// request which starts and finishes between two 5 s poll scrapes, but a counter read tightly
/// around the probe's own window cannot; (d) nothing else on the engine during it, PROVED from
/// the engine's own running/queue afterwards and, when there is a gateway, from its admitted
/// counters, as a cheap first filter (a genuinely busy engine rejects a probe before it is even
/// sent, and (c) still catches what this misses). What an engine does not report cannot be
/// proved, and must not be treated as a failure: without a gateway (the default for a new user)
/// and on an engine that publishes no load (Ollama, LM Studio, a plain OpenAI-compatible server)
/// the reading is kept with `CouldNotRuleOut`, and MODEL shows the median of many such readings
/// rather than nothing at all.
///
/// TRIED AND REMOVED (card #45, 2026-09-20, same day): a cold-GPU-clock check, invalidating a
/// probe whose GPU sat below 500 MHz right before it began. Live, this was not rare: a
/// power-managed GPU idles down to its own power-save clock in the ordinary gap between requests,
/// which is exactly the moment right before almost every genuinely-idle probe - the check
/// invalidated the large majority of readings across most hours, several hours to zero valid
/// probes, over a property the counter check alone was already keeping (66 of ~89 probes valid,
/// no hour at zero). It could not tell "the ramp corrupted this reading" from "the GPU is
/// behaving normally between requests," because the two look identical from one clock sample.
/// Caught and reverted before it shipped for more than a few minutes; the counter check (c) had
/// already caught every one of the cases the clock check was built for.
pub fn validate(e: &ProbeEvidence, max_ttft_s: f64) -> Result<Confidence, (&'static str, String)> {
    if e.running_before > 0.0 || e.queue_before > 0.0 {
        return Err((INVALID_BUSY_BEFORE, format!("running={} queue={} before the probe", e.running_before, e.queue_before)));
    }
    if e.ttft_ms > max_ttft_s * 1000.0 {
        return Err((INVALID_SLOW_TTFT, format!("TTFT {:.1} s > {max_ttft_s} s: the engine was prefilling someone else", e.ttft_ms / 1000.0)));
    }
    if let Some(idle) = e.ttft_baseline_ms.filter(|b| *b > 0.0) {
        if e.ttft_ms > C1_TTFT_CONTENDED_RATIO * idle {
            return Err((
                INVALID_CONTENDED,
                format!("TTFT {:.1} ms is {:.1}x the idle median {idle:.1} ms (limit {C1_TTFT_CONTENDED_RATIO}x): the engine was busy with someone else's request", e.ttft_ms, e.ttft_ms / idle),
            ));
        }
    }
    if let (Some(before), Some(after)) = (e.requests_before, e.requests_after) {
        let others = (after - before).max(0.0) - 1.0;
        if others >= 1.0 {
            return Err((INVALID_CONTENDED, format!("requests_total grew by {:.0}: {others:.0} other request(s) finished on this engine while the probe was out", after - before)));
        }
    }
    if let (Some(before), Some(after)) = (e.prompt_tokens_before, e.prompt_tokens_after) {
        let grown = (after - before).max(0.0);
        if grown > PROBE_PROMPT_MARGIN_TOKENS {
            return Err((INVALID_CONTENDED, format!("prompt_tokens_total grew by {grown:.0} - the probe's own prompt is a sentence: someone else's prompt was processed during the probe")));
        }
    }
    if let (Some(before), Some(after)) = (e.gen_tokens_before, e.gen_tokens_after) {
        let grown = (after - before).max(0.0);
        // card #266: the engine counts a request's tokens when it FINISHES, so the "after" read can
        // land before or after the probe's own answer is counted. Either way the growth must be
        // ~0 (not counted yet) or ~the probe's own answer - anything else is another request.
        // Before #266 only "more than the probe's own" was rejected, so a foreign request's 28
        // tokens read before the probe's 128 were counted passed as "less than ours".
        let own_not_yet = grown <= COUNTER_MARGIN_TOKENS;
        let own_only = (grown - e.probe_tokens).abs() <= COUNTER_MARGIN_TOKENS;
        if !own_not_yet && !own_only {
            let why = if grown > e.probe_tokens {
                format!("{:.0} more than the probe's own {:.0} tokens", grown - e.probe_tokens, e.probe_tokens)
            } else {
                format!("neither 0 nor the probe's own {:.0} tokens", e.probe_tokens)
            };
            return Err((INVALID_CONTENDED, format!("generation_tokens_total grew by {grown:.0}, {why}: something else generated on this engine during the probe")));
        }
    }
    let mut caveat = None;
    match (e.running_after, e.queue_after) {
        (Some(running), Some(queue)) => {
            if running > 1.0 || queue > 0.0 {
                return Err((INVALID_CONTENDED, format!("running={running} queue={queue} right after the probe")));
            }
        }
        // the engine says nothing about its load: nobody can prove this ran alone
        _ if !e.engine_reports_load => caveat = Some("the engine does not report how many requests are running"),
        // it normally does, and this time we could not read it: that IS a failure to check
        _ => return Err((INVALID_CONTENDED, "engine metrics unreadable after the probe: cannot show it ran alone".into())),
    }
    if !e.gateway {
        // no gateway: the engine's own before/after is all the evidence there is
        return Ok(match caveat {
            Some(why) => Confidence::CouldNotRuleOut(why),
            None => Confidence::RanAlone,
        });
    }
    let (Some(before), Some(after)) = (e.admitted_before, e.admitted_after) else {
        return Err((INVALID_CONTENDED, "gate admitted counters unreadable: cannot show it ran alone".into()));
    };
    // a counter that went backwards = the gate restarted mid-probe
    let others = after.checked_sub(before).map(|d| d.saturating_sub(u64::from(e.probe_admitted)));
    match others {
        Some(0) => Ok(match caveat {
            Some(why) => Confidence::CouldNotRuleOut(why),
            None => Confidence::RanAlone,
        }),
        Some(n) => Err((INVALID_CONTENDED, format!("the gate admitted {n} other request(s) during the probe"))),
        None => Err((INVALID_CONTENDED, "the gate restarted during the probe".into())),
    }
}

/// The C1 reading a batch of finished probes contributes to the rule engine: the newest VALID
/// `ok` one. An invalid probe contributes nothing - it neither counts as low nor resets a streak.
pub fn c1_reading(results: &[ProbeRecord]) -> Option<f64> {
    results.iter().rev().find(|p| p.is_reading()).and_then(|p| p.decode_tok_s)
}

/// What the collector's own probe marks its request with, so anyone reading the gate or the
/// engine can tell it from real traffic: header `X-LSS-Probe: 1`, body field `"user"`.
pub const PROBE_HEADER: &str = "X-LSS-Probe";
pub const PROBE_USER: &str = "lss-probe";

impl ProbeRecord {
    pub fn skipped_busy(ts: i64, running: f64, queue: f64) -> Self {
        Self {
            ts,
            status: "skipped_busy".into(),
            ttft_ms: None,
            decode_tok_s: None,
            tokens: None,
            detail: format!("running={running} queue={queue}"),
            http_status: None,
            valid: false,
            invalid_reason: Some(INVALID_BUSY_BEFORE.into()),
            unverified: false,
        }
    }
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    /// completed AND valid: the only kind of probe that is a C1 reading
    pub fn is_reading(&self) -> bool {
        self.is_ok() && self.valid
    }

    pub fn invalidate(&mut self, reason: &str, detail: String) {
        self.valid = false;
        self.invalid_reason = Some(reason.to_string());
        self.detail = detail;
    }

    /// The status the GATE booked for this probe in its audit log, or None when the probe never
    /// produced a gate log line (skipped, or the gate was unreachable). Rows written before
    /// `http_status` existed are read from `status`/`detail`. A client-side timeout is what the
    /// gate logs as 499 (client closed).
    pub fn gate_logged_status(&self) -> Option<u16> {
        if let Some(code) = self.http_status {
            return Some(code);
        }
        match self.status.as_str() {
            "ok" => Some(200),
            "timeout" => Some(499),
            "error" => match self.detail.strip_prefix("HTTP ") {
                Some(code) => code.trim().parse().ok(),
                None if self.detail.starts_with("stream") => Some(200),
                None => None,
            },
            _ => None,
        }
    }

    /// Did this probe pass the gate's admission control (and so count in its `admitted`)?
    /// Rejections are 4xx before admission; 2xx, 5xx and a 499 all happen after it.
    pub fn was_admitted(&self) -> bool {
        self.gate_logged_status().is_some_and(|c| !(400..499).contains(&c))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    /// A chunk that carried generated text (content or reasoning_content).
    Token,
    /// `usage.completion_tokens` from the final chunk.
    Usage(u32),
    Done,
    Other,
}

/// One line of an OpenAI-style `text/event-stream` body.
pub fn parse_sse_line(line: &str) -> SseEvent {
    let Some(data) = line.trim().strip_prefix("data:") else { return SseEvent::Other };
    let data = data.trim();
    if data == "[DONE]" {
        return SseEvent::Done;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return SseEvent::Other };
    let has_text = v["choices"].as_array().is_some_and(|choices| {
        choices.iter().any(|c| {
            ["content", "reasoning_content"]
                .iter()
                .any(|k| c["delta"][k].as_str().is_some_and(|s| !s.is_empty()))
                || c["text"].as_str().is_some_and(|s| !s.is_empty())
        })
    });
    if has_text {
        return SseEvent::Token;
    }
    match v["usage"]["completion_tokens"].as_u64() {
        Some(n) => SseEvent::Usage(n as u32),
        None => SseEvent::Other,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeTiming {
    pub ttft_ms: f64,
    pub decode_tok_s: f64,
    pub tokens: u32,
    /// how many text-bearing chunks carried them, and over how long: a speed measured over a
    /// window shorter than the tokens need is not a speed (see `burst_reason`)
    pub chunks: u32,
    pub span_ms: f64,
}

/// `chunk_ms` = arrival time of every text-bearing chunk, in ms since the request was sent.
/// `usage_tokens` = the server's own completion-token count when it sent one; with
/// speculative decoding a chunk can carry several tokens, so chunk count alone under-reads.
/// Decode rate is measured strictly AFTER the first token, so prefill time never pollutes it.
pub fn timing(chunk_ms: &[f64], usage_tokens: Option<u32>) -> Option<ProbeTiming> {
    let first = *chunk_ms.first()?;
    let last = *chunk_ms.last()?;
    let tokens = usage_tokens.unwrap_or(chunk_ms.len() as u32).max(chunk_ms.len() as u32);
    let span_s = (last - first) / 1000.0;
    if tokens < 2 || span_s <= 0.0 {
        return None;
    }
    Some(ProbeTiming { ttft_ms: first, decode_tok_s: f64::from(tokens - 1) / span_s, tokens, chunks: chunk_ms.len() as u32, span_ms: last - first })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-20 02:49 and 02:54, right after a gateway restart: the whole answer arrived in one
    /// piece after a long wait, and lss stored "26,773 tok/s" and "5,596 tok/s" as VALID readings.
    #[test]
    fn an_answer_that_arrived_in_one_burst_is_not_a_speed() {
        // 128 tokens in 2 chunks over 4.8 ms = 26,773 tok/s
        let burst = timing(&[2365.0, 2369.8], Some(128)).unwrap();
        assert!(burst.decode_tok_s > 26_000.0 && burst.chunks == 2);
        let why = burst_reason(&burst, Some(185.2)).expect("a burst");
        assert!(why.contains("arrived in one burst") && why.contains("128 tokens in 2 chunk(s)"), "{why}");
        // and it is caught even before a baseline has been learned
        assert!(burst_reason(&burst, None).is_some());
        // the 5,596 tok/s one: 128 tokens in 6 fat chunks over 22.7 ms
        let burst2 = timing(&[1500.0, 1504.0, 1508.0, 1512.0, 1518.0, 1522.7], Some(128)).unwrap();
        assert!(burst2.decode_tok_s > 5_000.0 && burst_reason(&burst2, Some(185.2)).is_some());
        // a real streamed answer at this server's usual speed is untouched
        let real: Vec<f64> = (0..128).map(|i| 140.0 + f64::from(i) * 5.2).collect();
        let good = timing(&real, Some(128)).unwrap();
        assert!((good.decode_tok_s - 192.0).abs() < 2.0, "{}", good.decode_tok_s);
        assert_eq!(burst_reason(&good, Some(185.2)), None);
        // a genuinely fast engine that really streams (many chunks) is untouched, with no baseline
        let fast: Vec<f64> = (0..128).map(|i| 30.0 + f64::from(i) * 0.4).collect();
        let fast = timing(&fast, Some(128)).unwrap();
        assert!(fast.decode_tok_s > 2_000.0 && burst_reason(&fast, None).is_none(), "{fast:?}");
        // ... but once the baseline is known, twice it is not a speed-up
        assert!(burst_reason(&fast, Some(185.2)).unwrap().contains("more than 2x the usual"));
    }

    /// #45 residual, 2026-09-20: `2026-09-20 22:17:58 | 445.3 tok/s | valid` became the MODEL
    /// page's "best" - streamed in properly-sized chunks (the burst/chunk test never fires), and
    /// 445.3 < 3x the 185.2 baseline (the old multiplier), so nothing caught it. The boundary
    /// that actually matters: 445.3 (2.40x) must die, 216.1 (1.17x, the next-fastest real
    /// reading ever recorded) must not - tightening this must never kill a genuinely fast answer.
    #[test]
    fn a_fast_tail_reading_dies_at_2x_baseline_a_merely_quick_one_does_not() {
        let streamed_at = |tok_s: f64| {
            let step_ms = 1000.0 / tok_s;
            let chunks: Vec<f64> = (1..=128).map(|i| f64::from(i) * step_ms).collect();
            timing(&chunks, Some(128)).unwrap()
        };
        let fast_tail = streamed_at(445.3);
        assert!((fast_tail.decode_tok_s - 445.3).abs() < 1.0 && fast_tail.chunks == 128, "{fast_tail:?}");
        let why = burst_reason(&fast_tail, Some(185.2)).expect("445.3 is 2.40x the 185.2 baseline");
        assert!(why.contains("more than 2x the usual 185"), "{why}");
        let next_fastest = streamed_at(216.1);
        assert_eq!(burst_reason(&next_fastest, Some(185.2)), None, "216.1 (1.17x) is a real fast reading, not a burst - it must survive");
    }

    fn gate(now: i64, last: Option<i64>, in_flight: bool, up: bool, running: f64, queue: f64) -> ProbeDecision {
        decide(&ProbeGate { now, last_attempt: last, interval_secs: 300, in_flight, serve_up: up, running, queue })
    }

    #[test]
    fn probe_runs_only_when_idle_and_due() {
        assert_eq!(gate(1000, None, false, true, 0.0, 0.0), ProbeDecision::Run);
        assert_eq!(gate(1000, Some(900), false, true, 0.0, 0.0), ProbeDecision::NotDue);
        assert_eq!(gate(1300, Some(1000), false, true, 0.0, 0.0), ProbeDecision::Run);
    }

    #[test]
    fn probe_is_skipped_when_busy() {
        assert_eq!(gate(1000, None, false, true, 1.0, 0.0), ProbeDecision::SkipBusy);
        assert_eq!(gate(1000, None, false, true, 0.0, 3.0), ProbeDecision::SkipBusy);
        let r = ProbeRecord::skipped_busy(1000, 1.0, 0.0);
        assert_eq!(r.status, "skipped_busy");
        assert!(!r.is_ok());
    }

    #[test]
    fn never_more_than_one_probe_and_never_against_a_down_serve() {
        assert_eq!(gate(9999, Some(0), true, true, 0.0, 0.0), ProbeDecision::InFlight);
        assert_eq!(gate(9999, Some(0), false, false, 0.0, 0.0), ProbeDecision::SkipDown);
    }

    #[test]
    fn what_the_gate_booked_for_a_probe() {
        let rec = |status: &str, detail: &str, http: Option<u16>| ProbeRecord { ts: 1, status: status.into(), ttft_ms: None, decode_tok_s: None, tokens: None, detail: detail.into(), http_status: http, valid: true, invalid_reason: None , unverified: false};
        assert_eq!(rec("ok", "", Some(200)).gate_logged_status(), Some(200));
        assert!(rec("ok", "", Some(200)).was_admitted());
        // rows from before http_status existed
        assert_eq!(rec("ok", "", None).gate_logged_status(), Some(200));
        assert_eq!(rec("error", "HTTP 429", None).gate_logged_status(), Some(429));
        assert!(!rec("error", "HTTP 429", None).was_admitted(), "rejected before admission");
        assert!(rec("error", "HTTP 502", None).was_admitted(), "upstream failed after admission");
        assert_eq!(rec("error", "stream too short to time (1 chunks)", None).gate_logged_status(), Some(200));
        assert_eq!(rec("timeout", "timed out", None).gate_logged_status(), Some(499));
        assert!(rec("timeout", "timed out", None).was_admitted());
        // never reached the gate: nothing to subtract anywhere
        assert_eq!(rec("error", "Connection refused", None).gate_logged_status(), None);
        assert_eq!(ProbeRecord::skipped_busy(1, 1.0, 0.0).gate_logged_status(), None);
        assert!(!ProbeRecord::skipped_busy(1, 1.0, 0.0).was_admitted());
    }

    fn idle() -> ProbeEvidence {
        ProbeEvidence { gateway: true, engine_reports_load: true, running_before: 0.0, queue_before: 0.0, ttft_ms: 140.0, running_after: Some(1.0), queue_after: Some(0.0), admitted_before: Some(60), admitted_after: Some(61), probe_admitted: true, gen_tokens_before: None, gen_tokens_after: None, probe_tokens: 0.0, ttft_baseline_ms: None, requests_before: None, requests_after: None, prompt_tokens_before: None, prompt_tokens_after: None }
    }

    #[test]
    fn a_probe_that_ran_alone_is_valid() {
        assert_eq!(validate(&idle(), 3.0), Ok(Confidence::RanAlone));
        // the probe's own request may already be gone from the engine
        assert_eq!(validate(&ProbeEvidence { running_after: Some(0.0), ..idle() }, 3.0), Ok(Confidence::RanAlone));
        // exactly at the TTFT limit is still fine
        assert_eq!(validate(&ProbeEvidence { ttft_ms: 3000.0, ..idle() }, 3.0), Ok(Confidence::RanAlone));
    }

    #[test]
    fn busy_before() {
        let (reason, detail) = validate(&ProbeEvidence { running_before: 1.0, ..idle() }, 3.0).unwrap_err();
        assert_eq!(reason, "busy_before");
        assert!(detail.contains("running=1"), "{detail}");
        assert_eq!(validate(&ProbeEvidence { queue_before: 2.0, ..idle() }, 3.0).unwrap_err().0, "busy_before");
        let skipped = ProbeRecord::skipped_busy(1, 1.0, 0.0);
        assert!(!skipped.valid && skipped.invalid_reason.as_deref() == Some("busy_before"));
    }

    #[test]
    fn slow_ttft() {
        // the two seen live on 2026-09-19
        for ms in [57_800.0, 26_548.4, 3000.1] {
            let (reason, detail) = validate(&ProbeEvidence { ttft_ms: ms, ..idle() }, 3.0).unwrap_err();
            assert_eq!(reason, "slow_ttft", "{detail}");
        }
        assert_eq!(validate(&ProbeEvidence { ttft_ms: 4000.0, ..idle() }, 5.0), Ok(Confidence::RanAlone), "the limit is config (c1_max_ttft_s)");
    }

    /// E1, 2026-09-20: with `gate_url = ""` - the DEFAULT for everyone who installs lss - every
    /// probe was stored INVALID ("gate admitted counters unreadable"), so C1 never learned and
    /// Ollama / LM Studio / a plain OpenAI server showed no writing speed at all, forever.
    #[test]
    fn without_a_gateway_a_probe_is_judged_on_the_engines_own_evidence() {
        let no_gate = ProbeEvidence { gateway: false, admitted_before: None, admitted_after: None, probe_admitted: false, ..idle() };
        // llama.cpp / SGLang / vLLM: they say how busy they are, so it can still be PROVED
        assert_eq!(validate(&no_gate, 3.0), Ok(Confidence::RanAlone));
        assert_eq!(validate(&ProbeEvidence { running_after: Some(3.0), ..no_gate.clone() }, 3.0).unwrap_err().0, "contended");
        assert_eq!(validate(&ProbeEvidence { running_before: 2.0, ..no_gate.clone() }, 3.0).unwrap_err().0, "busy_before");
        // Ollama / LM Studio / generic: nothing to prove it with, so it is kept WITH the caveat
        let blind = ProbeEvidence { engine_reports_load: false, running_after: None, queue_after: None, ..no_gate.clone() };
        assert_eq!(validate(&blind, 3.0), Ok(Confidence::CouldNotRuleOut("the engine does not report how many requests are running")));
        // a slow first word is still a refusal there: that evidence needs nobody else
        assert_eq!(validate(&ProbeEvidence { ttft_ms: 9_000.0, ..blind.clone() }, 3.0).unwrap_err().0, "slow_ttft");
        // an engine that normally reports its load but could not be read this time is NOT a reading
        assert_eq!(validate(&ProbeEvidence { running_after: None, queue_after: None, ..no_gate }, 3.0).unwrap_err().0, "contended");
        // with a gateway, its counters are still required (nothing about that changed)
        assert_eq!(validate(&ProbeEvidence { admitted_after: None, ..idle() }, 3.0).unwrap_err().0, "contended");
        // and a blind engine BEHIND a gateway is judged by the gateway, with the same caveat
        let blind_behind_gate = ProbeEvidence { engine_reports_load: false, running_after: None, queue_after: None, ..idle() };
        assert!(matches!(validate(&blind_behind_gate, 3.0), Ok(Confidence::CouldNotRuleOut(_))));
        assert_eq!(validate(&ProbeEvidence { admitted_after: Some(70), ..blind_behind_gate }, 3.0).unwrap_err().0, "contended");
    }

    #[test]
    fn contended() {
        // someone else is still decoding next to the probe
        assert_eq!(validate(&ProbeEvidence { running_after: Some(2.0), ..idle() }, 3.0).unwrap_err().0, "contended");
        assert_eq!(validate(&ProbeEvidence { queue_after: Some(1.0), ..idle() }, 3.0).unwrap_err().0, "contended");
        // a short request came and went inside the window: only the gate's counter shows it
        let (reason, detail) = validate(&ProbeEvidence { admitted_after: Some(63), ..idle() }, 3.0).unwrap_err();
        assert_eq!(reason, "contended");
        assert!(detail.contains("admitted 2 other request(s)"), "{detail}");
        // a probe the gate rejected accounts for none of the growth
        assert_eq!(validate(&ProbeEvidence { probe_admitted: false, ..idle() }, 3.0).unwrap_err().0, "contended");
        assert_eq!(validate(&ProbeEvidence { probe_admitted: false, admitted_after: Some(60), ..idle() }, 3.0), Ok(Confidence::RanAlone));
        // what cannot be verified does not count as idle
        assert_eq!(validate(&ProbeEvidence { admitted_after: None, ..idle() }, 3.0).unwrap_err().0, "contended");
        assert_eq!(validate(&ProbeEvidence { running_after: None, ..idle() }, 3.0).unwrap_err().0, "contended");
        assert_eq!(validate(&ProbeEvidence { admitted_after: Some(3), ..idle() }, 3.0).unwrap_err().1, "the gate restarted during the probe");
    }

    #[test]
    fn the_first_failed_condition_names_the_reason() {
        let e = ProbeEvidence { running_before: 2.0, ttft_ms: 9000.0, running_after: Some(4.0), ..idle() };
        assert_eq!(validate(&e, 3.0).unwrap_err().0, "busy_before");
        assert_eq!(validate(&ProbeEvidence { running_before: 0.0, ..e }, 3.0).unwrap_err().0, "slow_ttft");
    }

    /// #45, 2026-09-20: the live 08:45:11 case exactly. `running` read 0.0 the whole 5 s poll
    /// window (so the old gate said nothing was wrong) while `generation_tokens_total` advanced
    /// 50-534 tokens every step - another client's request, entirely between two poll scrapes.
    /// The counter is the only signal that catches it.
    #[test]
    fn a_counter_that_grew_more_than_the_probes_own_answer_is_contended_even_when_running_read_zero() {
        let e = ProbeEvidence { gen_tokens_before: Some(48_213.0), gen_tokens_after: Some(48_213.0 + 288.0), probe_tokens: 128.0, ..idle() };
        let (reason, detail) = validate(&e, 3.0).unwrap_err();
        assert_eq!(reason, "contended");
        assert!(detail.contains("grew by 288") && detail.contains("160 more") && detail.contains("128 tokens"), "{detail}");
        // the probe's own tokens, even right at the margin, are not contamination
        assert_eq!(validate(&ProbeEvidence { gen_tokens_after: Some(48_213.0 + 128.0 + COUNTER_MARGIN_TOKENS), ..e.clone() }, 3.0), Ok(Confidence::RanAlone));
        assert_eq!(validate(&ProbeEvidence { gen_tokens_after: Some(48_213.0 + 128.0 + COUNTER_MARGIN_TOKENS + 1.0), ..e }, 3.0).unwrap_err().0, "contended", "one token past the margin still counts");
        // an engine with no such counter (Ollama, LM Studio) keeps today's behaviour: skipped, not invented
        assert_eq!(validate(&idle(), 3.0), Ok(Confidence::RanAlone));
    }

    #[test]
    fn only_a_valid_ok_probe_is_a_c1_reading() {
        let ok = |v: f64| ProbeRecord { ts: 1, status: "ok".into(), ttft_ms: Some(140.0), decode_tok_s: Some(v), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false};
        let mut bad = ok(141.4);
        bad.invalidate("contended", "x".into());
        assert_eq!(c1_reading(&[ok(190.0), bad.clone()]), Some(190.0), "the invalid one is passed over, not used");
        assert_eq!(c1_reading(&[bad, ProbeRecord::skipped_busy(2, 1.0, 0.0)]), None);
        assert_eq!(c1_reading(&[]), None);
        // a document from before `valid` existed reads as valid
        let old: ProbeRecord = serde_json::from_str(r#"{"ts":1,"status":"ok","decode_tok_s":188.0}"#).unwrap();
        assert!(old.is_reading());
    }

    #[test]
    fn sse_lines() {
        assert_eq!(parse_sse_line(r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#), SseEvent::Token);
        assert_eq!(parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning_content":"hm"}}]}"#), SseEvent::Token);
        assert_eq!(parse_sse_line(r#"data: {"choices":[{"delta":{"role":"assistant","content":""}}]}"#), SseEvent::Other);
        assert_eq!(parse_sse_line(r#"data: {"choices":[],"usage":{"completion_tokens":128}}"#), SseEvent::Usage(128));
        assert_eq!(parse_sse_line("data: [DONE]"), SseEvent::Done);
        assert_eq!(parse_sse_line(": keep-alive"), SseEvent::Other);
    }

    #[test]
    fn timing_excludes_prefill_and_prefers_server_token_count() {
        // first token at 200 ms, last at 1200 ms, 40 chunks but 128 tokens (spec decode)
        let chunks: Vec<f64> = (0..40).map(|i| 200.0 + f64::from(i) * (1000.0 / 39.0)).collect();
        let t = timing(&chunks, Some(128)).unwrap();
        assert_eq!(t.ttft_ms, 200.0);
        assert_eq!(t.tokens, 128);
        assert!((t.decode_tok_s - 127.0).abs() < 1e-6);
        let t = timing(&chunks, None).unwrap();
        assert!((t.decode_tok_s - 39.0).abs() < 1e-6);
        assert!(timing(&[], None).is_none());
        assert!(timing(&[100.0], Some(1)).is_none());
    }
}
