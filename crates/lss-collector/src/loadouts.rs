//! The loadout the box is serving right now, and the scorecards of the ones before it.
//! Identity comes from `docker inspect` (image, launch args, serving environment) plus the served
//! model id. A restart of the SAME configuration is the same loadout (one more run); a changed
//! arg, image, model or serving switch is a new one. The accumulator is saved once a minute, so
//! a collector restart carries on where it left off.

use crate::db::Db;
use lss_core::bench::Scorecard;
use lss_core::compare::{LoadoutCard, LoadoutsDoc};
use lss_core::hist::{hist_index, ClosedWindow};
use lss_core::loadout::{row, LoadoutAcc, LoadoutIdentity};
use lss_core::model::{LoadoutBrief, Sample};
use lss_core::probe::ProbeRecord;

const SAVE_EVERY_SECS: i64 = 60;
const SCORECARD_ROWS: u32 = 30;
const BENCH_ROWS: u32 = 300;

struct Current {
    id: LoadoutIdentity,
    acc: LoadoutAcc,
    first_seen: i64,
    /// (container name, started_at, model) this identity was read for
    seen_as: (String, i64, String),
}

#[derive(Default)]
pub struct Loadouts {
    current: Option<Current>,
    /// every loadout on record, newest first (reloaded when the current one changes)
    past: Vec<(LoadoutIdentity, LoadoutAcc, i64)>,
    /// every bench run on record, newest first (reloaded when a bench finishes)
    bench: Vec<Scorecard>,
    saved_at: i64,
    prev: Option<Sample>,
    /// the newest poll in which the serve did NOT answer (a cold start is only timed when the
    /// collector watched it being down)
    last_down_ts: Option<i64>,
    /// `electricity_usd_per_kwh` and `targets.min_tok_s_per_user` from the config
    pub usd_per_kwh: Option<f64>,
    pub min_tok_s_per_user: f64,
}

/// #105, 2026-09-22 (verifier): `electricity_usd_per_kwh` is ALWAYS a flat number, by design -
/// on a real time-of-use plan that is exactly the 2x+ lie card #75's engine exists to prevent.
/// Once a real `[rates] path` is configured, that engine is authoritative (it carries an
/// effective date and states what it excludes; this old card-#6 path carries neither) - this
/// suppresses the old flat figure entirely rather than risk a second, uncaveated money figure
/// sitting next to it. Pure so the gate itself, not just its plumbing, is directly tested.
pub fn effective_usd_per_kwh(flat: Option<f64>, rates_configured: bool) -> Option<f64> {
    if rates_configured {
        None
    } else {
        flat
    }
}

impl Loadouts {
    pub fn load(db: &Db) -> Self {
        Self { past: db.recent_loadouts(SCORECARD_ROWS).unwrap_or_default(), bench: db.bench_runs(BENCH_ROWS).unwrap_or_default(), ..Default::default() }
    }

    /// A bench run started or finished: read the runs again.
    pub fn reload_bench(&mut self, db: &Db) {
        self.bench = db.bench_runs(BENCH_ROWS).unwrap_or_default();
    }

    pub fn current_id(&self) -> Option<&LoadoutIdentity> {
        self.current.as_ref().map(|c| &c.id)
    }

    /// The current loadout's long-run reading speed (prompt tokens per second while reading).
    pub fn prefill_typical(&self) -> Option<f64> {
        self.current.as_ref().and_then(|c| row(&c.id, &c.acc, c.first_seen, true).prefill_tok_s)
    }

    /// #50, 2026-09-21: the live, concurrency-bucketed decode speed at `running` requests at
    /// once - passively observed from real traffic (`LoadoutAcc::observe` -> `Curve::observe`
    /// feeds this every 5s sample, benchmark or not; nothing here runs a probe or needs the
    /// engine idle). `None` before anything has been observed at exactly this level yet, or
    /// before a loadout is known at all - never a fabricated number for a level nobody has seen.
    pub fn live_speed(&self, running: f64) -> Option<lss_core::loadout::CurveRow> {
        let c = self.current.as_ref()?;
        let level = running.round().max(1.0) as u64;
        c.acc.curve.rows(c.acc.slots.max(1) as usize).into_iter().find(|r| r.running == level && r.samples > 0)
    }

    pub fn brief(&self) -> Option<LoadoutBrief> {
        self.current.as_ref().map(|c| LoadoutBrief { id: c.id.id.clone(), model: c.id.model.clone(), image_tag: c.id.image_tag(), first_seen: c.first_seen, runs: c.acc.runs, flags: c.id.flags.clone() })
    }

    /// What identifies what is serving in this sample, once it is up and answering: the serve
    /// CONTAINER when there is one, else the engine itself (an empty name) - a plain process is
    /// how Ollama, LM Studio and llama-server normally run, and how any machine without docker
    /// runs everything.
    fn seen_as(sample: &Sample) -> Option<(String, i64, String)> {
        let model = sample.model.clone().filter(|_| sample.serve_up && sample.model.as_deref() != Some(lss_core::engine::NO_MODEL))?;
        match sample.serve_ct.as_ref().filter(|c| c.running() && c.started_at > 0) {
            Some(ct) => Some((ct.name.clone(), ct.started_at, model)),
            None => Some((String::new(), 0, model)),
        }
    }

    /// One poll. `identify` is only called when another container run, another engine or
    /// another model shows up; its argument is the container name, or "" when the engine is not
    /// a container and must describe itself.
    pub fn on_sample(&mut self, db: &Db, sample: &Sample, identify: impl FnOnce(&str) -> Option<LoadoutIdentity>) {
        if !sample.serve_up {
            self.last_down_ts = Some(sample.ts);
        }
        if let Some(seen) = Self::seen_as(sample) {
            if self.current.as_ref().is_none_or(|c| c.seen_as != seen) {
                match identify(&seen.0) {
                    Some(id) => self.switch_to(db, id, seen, sample.ts),
                    None if seen.0.is_empty() => eprintln!("loadout: the engine did not describe itself; will ask again"),
                    None => eprintln!("loadout: docker inspect of {} gave nothing; will ask again", seen.0),
                }
            }
        }
        if let Some(c) = &mut self.current {
            c.acc.slots = c.acc.slots.max(u64::from(sample.slots));
            c.acc.observe(self.prev.as_ref(), sample);
            if sample.ts - self.saved_at >= SAVE_EVERY_SECS {
                self.saved_at = sample.ts;
                if let Err(e) = db.save_loadout(&c.id, &c.acc, c.first_seen) {
                    eprintln!("db: loadout write failed: {e}");
                }
            }
        }
        self.prev = Some(sample.clone());
    }

    fn switch_to(&mut self, db: &Db, id: LoadoutIdentity, seen_as: (String, i64, String), now: i64) {
        let started_at = seen_as.1;
        // COLD START: container start -> this first answer, when the collector saw it down in
        // between (a collector that started later cannot know when the serve became ready)
        let cold_start = self.last_down_ts.filter(|d| *d >= started_at && now - *d <= 60).map(|_| (now - started_at) as f64);
        if let Some(cur) = self.current.as_mut().filter(|c| c.id.id == id.id) {
            // the same configuration started again: the same loadout, one more run
            cur.acc.note_run(started_at);
            if let Some(secs) = cold_start {
                cur.acc.note_cold_start(secs);
                eprintln!("loadout: cold start took {secs:.0} s (container start to first answer)");
            }
            cur.seen_as = seen_as;
            eprintln!("loadout: {} restarted with the same configuration (run {})", cur.id.id, cur.acc.runs);
            self.prev = None;
            return;
        }
        if let Some(old) = self.current.take() {
            let _ = db.save_loadout(&old.id, &old.acc, old.first_seen);
        }
        let (mut acc, first_seen) = match db.load_loadout(&id.id) {
            Ok(Some(found)) => {
                eprintln!("loadout: BACK to {} ({} · {}), first seen {}", id.id, id.model, id.image_tag(), found.1);
                found
            }
            _ => {
                // never seen: what the database already holds about this container run counts
                let acc = backfill(db, started_at, now);
                eprintln!("loadout: NEW {} · {} · {} · config {} ({} stored samples folded in)", id.id, id.model, id.image, &id.args_hash[..12], acc.samples);
                // first seen = when that container started, if the database reaches back that far.
                // D1, 2026-09-20: a plain process has no container start time (`started_at == 0`),
                // so this used to publish 1970-01-01 to everyone the moment any sample already
                // existed - which for Ollama/LM Studio (loaded on demand) is EVERY time. `started_at`
                // only counts when it is a real timestamp.
                let first_seen = if acc.samples > 0 && started_at > 0 { started_at.min(now) } else { now };
                (acc, first_seen)
            }
        };
        acc.note_run(started_at);
        if let Some(secs) = cold_start {
            acc.note_cold_start(secs);
            eprintln!("loadout: cold start took {secs:.0} s (container start to first answer)");
        }
        let _ = db.save_loadout(&id, &acc, first_seen);
        self.current = Some(Current { id, acc, first_seen, seen_as });
        self.saved_at = now;
        // no `prev` across a switch: the first interval of a run is not integrated
        self.prev = None;
        self.past = db.recent_loadouts(SCORECARD_ROWS).unwrap_or_default();
    }

    pub fn on_probe(&mut self, p: &ProbeRecord) {
        if let Some(c) = &mut self.current {
            c.acc.observe_probe(p);
        }
    }

    pub fn on_window(&mut self, w: &ClosedWindow) {
        if let Some(c) = &mut self.current {
            c.acc.observe_window(w);
        }
    }

    fn card(&self, id: &LoadoutIdentity, acc: &LoadoutAcc, first_seen: i64, current: bool) -> LoadoutCard {
        let runs = || self.bench.iter().filter(|b| b.loadout_id == id.id);
        let newest_ok = |profile: &str| runs().find(|b| b.profile == profile && b.complete()).cloned();
        let last_at = |profile: &str| runs().find(|b| b.profile == profile && b.status != "running").map(|b| b.ended_at);
        // the newest complete accuracy run of each dataset
        let mut accuracy: Vec<Scorecard> = Vec::new();
        for b in runs().filter(|b| b.profile == "accuracy" && b.complete() && !b.accuracy.is_empty()) {
            if !accuracy.iter().any(|a| a.accuracy[0].dataset == b.accuracy[0].dataset) {
                accuracy.push(b.clone());
            }
        }
        let mut row = row(id, acc, first_seen, current);
        row.apply_price(self.usd_per_kwh);
        let mut card = LoadoutCard { row, quick: newest_ok("quick"), full: newest_ok("full"), accuracy, last_quick_at: last_at("quick"), last_full_at: last_at("full"), last_accuracy_at: last_at("accuracy"), headroom: None };
        card.headroom = Some(card.estimate_headroom(self.min_tok_s_per_user));
        card
    }

    /// The current loadout's card (for the advice rules).
    pub fn current_card(&self) -> Option<LoadoutCard> {
        self.current.as_ref().map(|c| self.card(&c.id, &c.acc, c.first_seen, true))
    }

    pub fn doc(&self, now: i64) -> LoadoutsDoc {
        let mut cards: Vec<LoadoutCard> = Vec::new();
        if let Some(c) = &self.current {
            cards.push(self.card(&c.id, &c.acc, c.first_seen, true));
        }
        let current_id = self.current.as_ref().map(|c| c.id.id.clone());
        cards.extend(self.past.iter().filter(|(id, _, _)| Some(&id.id) != current_id.as_ref()).map(|(id, acc, first)| self.card(id, acc, *first, false)));
        LoadoutsDoc { v: lss_core::STATUS_SCHEMA_VERSION, generated_at: now, loadouts: cards }
    }
}

/// Everything already stored about the container run that started at `from`: raw samples,
/// probes and the 1-minute histogram windows.
fn backfill(db: &Db, from: i64, now: i64) -> LoadoutAcc {
    let mut acc = LoadoutAcc::default();
    let mut prev: Option<Sample> = None;
    let _ = db.each_sample(from, now, |s| {
        acc.slots = acc.slots.max(u64::from(s.slots));
        acc.observe(prev.as_ref(), &s);
        prev = Some(s);
    });
    for p in db.probes_between(from, now).unwrap_or_default() {
        acc.observe_probe(&p);
    }
    for (short, slot) in [("ttft", 0), ("itl", 2), ("prompt_tokens", 4), ("gen_tokens", 5)] {
        debug_assert_eq!(hist_index(short), Some(slot));
        let _ = db.hist_rows(60, short, from, now, |ts, h| {
            let mut accs: [lss_core::hist::HistAccum; lss_core::hist::N_HIST] = Default::default();
            accs[slot] = h;
            acc.observe_window(&ClosedWindow { res: 60, ts, accs });
        });
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use lss_core::docker::ContainerState;
    use lss_core::gpu::GpuSample;
    use lss_core::loadout::Inspected;
    use lss_core::prom::ServeMetrics;

    /// #105 (verifier): a real `[rates] path` suppresses the old flat figure entirely, even
    /// when `electricity_usd_per_kwh` is ALSO set - never a second, uncaveated money figure
    /// beside #75's engine.
    #[test]
    fn a_configured_rates_table_suppresses_the_old_flat_figure_even_if_both_are_set() {
        assert_eq!(effective_usd_per_kwh(Some(0.30), true), None, "[rates] configured: the old path must go quiet, not print alongside the new engine");
        assert_eq!(effective_usd_per_kwh(Some(0.30), false), Some(0.30), "no [rates]: the flat figure is all there is, so it still shows");
        assert_eq!(effective_usd_per_kwh(None, false), None, "neither configured: nothing to show either way");
        assert_eq!(effective_usd_per_kwh(None, true), None);
    }

    fn sample(ts: i64, container_started: i64, model: &str, gen_total: f64) -> Sample {
        Sample {
            ts,
            serve_up: true,
            model: Some(model.into()),
            metrics: Some(ServeMetrics { running: 2.0, gen_throughput: 380.0, generation_tokens_total: gen_total, ..Default::default() }),
            gpus_ok: true,
            gpus: vec![GpuSample { index: 0, power_w: Some(800.0), ..Default::default() }],
            serve_ct: Some(ContainerState { name: "serve".into(), status: "running".into(), restart_count: 0, started_at: container_started }),
            slots: 8,
            ..Default::default()
        }
    }

    fn inspected(tp: &str) -> Option<Inspected> {
        Some(Inspected { image: "repo/serve:tag-1".into(), args: vec!["python3".into(), "--tp-size".into(), tp.into()], env: vec!["SGLANG_X=1".into(), "PATH=/bin".into()] })
    }

    /// what the collector does with `docker inspect` output
    fn args(tp: &str) -> Option<LoadoutIdentity> {
        let i = inspected(tp)?;
        Some(lss_core::loadout::identity("glm", &i.image, &i.args, &i.env))
    }

    /// E2: an engine that is not a container (a plain-process Ollama / llama-server / LM Studio,
    /// or any machine without docker) used to get NO loadout at all, so MODEL, `lss loadouts`
    /// and every bench scorecard stayed empty.
    #[test]
    fn an_engine_that_is_not_a_container_gets_a_loadout_too() {
        let db = Db::memory();
        let mut l = Loadouts::load(&db);
        let engine = lss_core::engine::EngineIdentity { model: Some("mistral:latest".into()), version: Some("0.34.2".into()), context_len: Some(8192.0), slots: None, detail: "7.2B · Q4_0".into(), cmdline: String::new() };
        let plain = |ts: i64, gen: f64| Sample { serve_ct: None, model: Some("mistral:latest".into()), ..sample(ts, 0, "mistral:latest", gen) };
        let mut asked = 0;
        for k in 0..30 {
            l.on_sample(&db, &plain(1000 + k * 5, (k * 100) as f64), |name| {
                asked += 1;
                assert_eq!(name, "", "no container: the engine describes itself");
                Some(lss_core::loadout::identity_from_engine("ollama", &engine))
            });
        }
        assert_eq!(asked, 1, "asked once per engine run, not once per poll");
        let card = l.doc(2000).loadouts[0].clone();
        assert_eq!((card.row.model.as_str(), card.row.image_tag.as_str(), card.row.current), ("mistral:latest", "0.34.2", true));
        assert!(card.row.flags.contains("ctx 8192"), "{}", card.row.flags);
        assert!(l.brief().is_some(), "the MODEL page and a bench scorecard have a loadout to hang on");

        // a finished bench on THIS loadout shows on MODEL / loadouts (attachment is purely by
        // loadout_id, so it needs nothing container-specific)
        let loadout_id = card.row.id.clone();
        let finished = Scorecard { run_id: 0, loadout_id: loadout_id.clone(), model: "mistral:latest".into(), profile: "quick".into(), started_at: 1_900, ended_at: 1_990, status: "ok".into(), ..Default::default() };
        let run_db_id = db.bench_start(&loadout_id, "quick", 1_900, &finished).unwrap();
        db.bench_finish(run_db_id, &finished).unwrap();
        l.reload_bench(&db);
        let card = l.doc(2000).loadouts[0].clone();
        assert!(card.quick.as_ref().is_some_and(|s| s.status == "ok"), "the bench scorecard is on the non-container loadout's card");
        // nothing loaded yet (Ollama before its first request) is not a loadout
        let mut empty = Loadouts::load(&Db::memory());
        let idle = Sample { serve_ct: None, model: Some(lss_core::engine::NO_MODEL.into()), ..sample(1000, 0, "x", 0.0) };
        empty.on_sample(&db, &idle, |_| panic!("nothing is loaded: nothing to identify"));
        assert!(empty.brief().is_none());
    }

    /// D1, 2026-09-20: a plain process has no container start time (`seen_as.1 == 0`), and
    /// `backfill` scans the WHOLE samples table (`from = started_at = 0`) - so the moment any
    /// sample from anything already sits in the database, `acc.samples > 0` on the very first
    /// `switch_to` for a plain-process loadout. The old rule then published `first_seen = 0`
    /// (1970-01-01) to everyone: guaranteed for Ollama / LM Studio, which load on demand, so a
    /// poll from before the model was known is already stored by the time it is.
    #[test]
    fn a_plain_process_loadout_created_after_prior_samples_exist_is_not_born_in_1970() {
        let db = Db::memory();
        // some unrelated sample already sits in the database (a poll before this engine loaded,
        // or from a collector that watched something else first) - nothing to do with this loadout
        db.insert_sample(&sample(500, 0, lss_core::engine::NO_MODEL, 0.0)).unwrap();
        let mut l = Loadouts::load(&db);
        let engine = lss_core::engine::EngineIdentity { model: Some("smollm:135m".into()), version: Some("0.34.2".into()), context_len: Some(2048.0), slots: None, detail: "0.1B · Q4_0".into(), cmdline: String::new() };
        let plain = |ts: i64, gen: f64| Sample { serve_ct: None, model: Some("smollm:135m".into()), ..sample(ts, 0, "smollm:135m", gen) };
        l.on_sample(&db, &plain(1000, 0.0), |_| Some(lss_core::loadout::identity_from_engine("ollama", &engine)));
        let brief = l.brief().expect("a loadout now exists");
        assert_eq!(brief.first_seen, 1000, "first_seen must be `now` (this poll), never 0 - a real started_at was never known");
    }

    #[test]
    fn a_loadout_accumulates_across_restarts_and_a_changed_arg_is_a_new_one() {
        let db = Db::memory();
        let mut l = Loadouts::load(&db);
        let mut inspected = 0;
        for k in 0..30 {
            l.on_sample(&db, &sample(1000 + k * 5, 900, "glm", (k * 1900) as f64), |_| {
                inspected += 1;
                args("4")
            });
        }
        assert_eq!(inspected, 1, "docker inspect runs once per container run, not once per poll");
        let before = l.doc(2000).loadouts[0].clone();
        assert_eq!((before.row.model.as_str(), before.row.current, before.row.gen_tokens, before.row.flags.as_str(), before.row.image_tag.as_str(), before.row.runs, before.row.slots), ("glm", true, 29.0 * 1900.0, "tp 4", "tag-1", 1, 8));
        assert_eq!(l.brief().map(|b| (b.model, b.image_tag, b.runs)), Some(("glm".into(), "tag-1".into(), 1)));
        drop(l); // the COLLECTOR restarts: up to a minute of accumulation is the most that is lost

        let mut l = Loadouts::load(&db);
        assert!(l.doc(2000).loadouts.iter().all(|r| !r.row.current), "nothing is current until the serve is seen again");
        for k in 30..60 {
            l.on_sample(&db, &sample(1000 + k * 5, 900, "glm", (k * 1900) as f64), |_| args("4"));
        }
        let doc = l.doc(3000);
        assert_eq!(doc.loadouts.len(), 1, "the same configuration is the same row");
        let after = &doc.loadouts[0].row;
        assert_eq!((after.id.as_str(), after.runs), (before.row.id.as_str(), 1));
        assert!(after.gen_tokens >= 50.0 * 1900.0 && after.gen_tokens <= 59.0 * 1900.0, "{}", after.gen_tokens);

        // the SERVE restarts with the same configuration: the same loadout, one more run
        let tokens = after.gen_tokens;
        for k in 60..70 {
            l.on_sample(&db, &sample(1000 + k * 5, 1290, "glm", ((k - 60) * 1900) as f64), |_| args("4"));
        }
        let doc = l.doc(3500);
        assert_eq!((doc.loadouts.len(), doc.loadouts[0].row.runs, doc.loadouts[0].row.started_at), (1, 2, 1290));
        assert_eq!(doc.loadouts[0].row.gen_tokens, tokens + 9.0 * 1900.0, "the step across the restart is a gap; the counters starting from zero again add nothing negative");

        // one arg changed: a NEW loadout; the old one is kept for comparison
        for k in 70..80 {
            l.on_sample(&db, &sample(1000 + k * 5, 1350, "glm", (k * 10) as f64), |_| args("8"));
        }
        let doc = l.doc(4000);
        let view: Vec<(&str, bool, &str)> = doc.loadouts.iter().map(|r| (r.row.model.as_str(), r.row.current, r.row.flags.as_str())).collect();
        assert_eq!(view, vec![("glm", true, "tp 8"), ("glm", false, "tp 4")]);
        assert_ne!(doc.loadouts[0].row.id, doc.loadouts[1].row.id);
        // ... and going BACK to the first configuration picks its row up again
        for k in 80..85 {
            l.on_sample(&db, &sample(1000 + k * 5, 1400, "glm", (k * 10) as f64), |_| args("4"));
        }
        let doc = l.doc(5000);
        assert_eq!((doc.loadouts.len(), doc.loadouts[0].row.flags.as_str(), doc.loadouts[0].row.runs), (2, "tp 4", 3));
        assert!(doc.loadouts[0].row.gen_tokens > tokens, "it carried on from what it had");
        // while the serve is down nothing switches, and the downtime is the current loadout's
        let mut down = sample(1500, 1400, "glm", 0.0);
        down.serve_up = false;
        down.model = None;
        down.metrics = None;
        l.on_sample(&db, &down, |_| panic!("no inspect while it is down"));
        assert!(l.doc(5000).loadouts[0].row.uptime_pct.unwrap() < 100.0);
    }

    /// #50, 2026-09-21: `live_speed` is the collector's read side of the passively-observed
    /// concurrency curve - prove it buckets by how many requests were in flight (never blends
    /// two levels into one number) and stays honestly `None` for a level nobody has been seen
    /// running at, rather than guessing.
    #[test]
    fn live_speed_is_bucketed_by_concurrency_and_none_where_unobserved() {
        let db = Db::memory();
        let mut l = Loadouts::load(&db);
        let at = |ts: i64, running: f64, gen_throughput: f64, gen_total: f64| Sample {
            metrics: Some(ServeMetrics { running, gen_throughput, generation_tokens_total: gen_total, ..Default::default() }),
            ..sample(ts, 900, "glm", gen_total)
        };
        // 2 requests in flight: aggregate settles around 200 tok/s -> 100 tok/s/req
        for k in 0..10 {
            l.on_sample(&db, &at(1000 + k * 5, 2.0, 200.0, k as f64 * 1000.0), |_| args("4"));
        }
        // 4 requests in flight: aggregate settles around 320 tok/s -> 80 tok/s/req each (more
        // users sharing the engine, less each - the thing a raw aggregate number would hide)
        for k in 10..20 {
            l.on_sample(&db, &at(1000 + k * 5, 4.0, 320.0, k as f64 * 1000.0), |_| args("4"));
        }
        let at2 = l.live_speed(2.0).expect("2 in flight was observed");
        assert_eq!((at2.running, at2.tok_s, at2.per_request_tok_s), (2, Some(200.0), Some(100.0)));
        let at4 = l.live_speed(4.0).expect("4 in flight was observed");
        assert_eq!((at4.running, at4.tok_s, at4.per_request_tok_s), (4, Some(320.0), Some(80.0)));
        assert_ne!(at2.per_request_tok_s, at4.per_request_tok_s, "different concurrency levels are never blended into one number");
        assert!(l.live_speed(7.0).is_none(), "nothing was ever seen at 7 in flight - never fabricate a reading for a level nobody hit");
        // rounding: a fractional `running` (the engine's own gauge, not always an integer at the
        // instant it is scraped) still finds the bucket it rounds to
        assert_eq!(l.live_speed(3.6).map(|r| r.running), Some(4), "3.6 rounds to the 4-in-flight bucket");
    }

    #[test]
    fn a_new_loadout_starts_from_what_the_database_already_knows_about_that_run() {
        let db = Db::memory();
        for k in 0..20 {
            db.insert_sample(&sample(5000 + k * 5, 4990, "glm", (k * 1000) as f64)).unwrap();
        }
        db.insert_sample(&sample(4000, 3000, "older-run", 5.0)).unwrap();
        db.insert_probe(&ProbeRecord { ts: 5050, status: "ok".into(), ttft_ms: Some(90.0), decode_tok_s: Some(191.0), tokens: Some(128), detail: String::new(), http_status: Some(200), valid: true, invalid_reason: None , unverified: false}).unwrap();
        let mut l = Loadouts::load(&db);
        l.on_sample(&db, &sample(5100, 4990, "glm", 20_000.0), |_| args("4"));
        let r = &l.doc(5100).loadouts[0].row;
        assert_eq!((r.gen_tokens, r.c1_tok_s, r.c1_probes), (19_000.0, Some(191.0), 1), "the stored samples since the container started, and nothing from before it");
        assert_eq!(r.first_seen, 4990, "first seen = when that container started, not when the collector was upgraded");
    }

    #[test]
    fn bench_runs_attach_to_their_loadout_and_an_aborted_run_is_never_the_scorecard() {
        let db = Db::memory();
        let mut l = Loadouts::load(&db);
        l.on_sample(&db, &sample(1000, 900, "glm", 0.0), |_| args("4"));
        let id = l.current_id().unwrap().id.clone();
        let card = |profile: &str, status: &str, ended: i64| Scorecard { loadout_id: id.clone(), profile: profile.into(), status: status.into(), started_at: ended - 60, ended_at: ended, ..Default::default() };
        for c in [card("quick", "ok", 2000), card("quick", "aborted", 3000), card("full", "ok", 2500)] {
            let run = db.bench_start(&id, &c.profile, c.started_at, &c).unwrap();
            db.bench_finish(run, &c).unwrap();
        }
        let other = Scorecard { loadout_id: "someone-else".into(), profile: "quick".into(), status: "ok".into(), ..Default::default() };
        db.bench_start("someone-else", "quick", 1, &other).unwrap();
        l.reload_bench(&db);
        let c = &l.doc(4000).loadouts[0];
        assert_eq!((c.quick.as_ref().map(|q| q.ended_at), c.full.as_ref().map(|f| f.ended_at)), (Some(2000), Some(2500)), "the newest COMPLETE run of each profile");
        assert_eq!((c.last_quick_at, c.last_full_at, c.last_accuracy_at), (Some(3000), Some(2500), None), "but the date shown is the last attempt");
        assert_eq!(c.speed().map(|s| s.profile.as_str()), Some("full"));
        // a run the collector died under is closed as aborted on the next start
        db.bench_start(&id, "quick", 3900, &card("quick", "running", 0)).unwrap();
        assert_eq!(db.bench_close_stale(4000, "aborted: the collector restarted").unwrap(), 2);
        assert!(db.bench_runs(10).unwrap().iter().all(|r| r.status != "running"));
    }
}
