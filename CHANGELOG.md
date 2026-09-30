# Changelog

## v1.3.0 (2026-09-29)

Plug-and-play. #512, #513, #514, #515, #516.

- **#512: the README opens with the Quickstart** — install fence + demo screenshot (lss-demo.svg)
  right after the intro paragraph, so a new reader sees how to run it before anything else.
- **#515: `lss setup` can point at a server on another machine.** The engine menu gains
  `r = the server is on another machine`; a typed address is live-tested before anything is
  written, and an unreachable one is refused in one plain line.
- **#516: the cost wizard asks the COUNTRY first.** Outside the US you type your electricity
  price per kWh from your bill (decimal commas and thousands separators understood), the currency
  is inferred for 49 countries or asked once, and both land in `rates.toml`.
- **#513: the gateway pages are hidden when there is no gateway.** A stock SGLang/vLLM/Ollama
  install shows only the pages it has; the keys `4`/`7` and `lss users`/`lss gateway` print the
  plain status.
- **#514: the omp watcher is opt-in.** Nothing reads or parses an omp config unless the operator
  sets `omp_config_path`; a box without omp is never watched.

## v1.2.2 (2026-09-26)

Fixes and docs since v1.2.1. 143 commits, 136 card commits. Most work in this window was the
private gateway's admission/charge machinery (v5.13-v5.15, `gate/`); the exported copy carries the
items below.

### Collector

- **`slots` re-read on serve identity change (#414).** The collector read slots once and kept them
  forever, so a serve restart that changed `max_running_requests` showed the old value (seen live:
  3/3 shown while the engine ran 8). Slots are now re-read whenever the served model id or the
  serve container's start time changes, and at least every 300 s — `LSS_SLOTS_RECHECK_SECS`
  overrides the recheck interval (`0` = every poll). A configured `slots` value is never
  re-fetched; a failed fetch drops the value to 0 and is retried next poll, never kept as truth.
  Documented on the `slots` row of the collector config reference.
- **Dead upstream isolation (#411, gateway side, but the collector's `/gate/health` view is what
  shows it):** a dead upstream (black-holed address, e.g. a powered-down second box) now answers
  503 within ~1 s while the live arm keeps serving — every concurrent miss for a dead arm's model
  shares one in-flight refresh, so nothing piles up behind a dead connect. The per-upstream health
  of each arm is published on `/gate/health`.
- **Budget and slot release on every exit (#382, gateway):** an admission that raises or is
  cancelled mid-wait now releases its trusted budget, user in-flight, key/lane slots and caller
  share exactly once, before the 429/response; `would_refuse_drain` counts only admission verdicts
  that were actually written.
- **Honest prefill-state after a gate restart (#384, gateway):** the charge-error reason is written
  exactly once (the init assignment and the except assignment were each single-line equivalent
  mutants), and a just-restarted gate reports `unmeasured` instead of a false `stalled` refusal
  while no prefill rate has been measured yet.
- **Per-caller cap shadow (#345, gateway, SHADOW):** keyless trusted callers are capped by source
  IP with a hard-bounded LRU; caller identity is the peer address, never a spoofable header.
- **Trie over-credit on one basis (#354, gateway):** trie credit is converted to the engine's
  reported basis before subtracting cached tokens, and the estimate now rides each admission record
  (`charged_trie_est`) with its own `request_id`.
- **True queued-uncached ledger (#362, gateway):** the queued figure is the measured ledger
  (admission estimate minus confirmed trie credit, released at outcome) with an exactly-once
  release on every exit; the would-refuse drain counter counts only written verdicts.
- **Plant-guard hardening (#374, #385):** the export's plant guard also flags hidden-concat
  spellings (f-string placeholders, `join`, `%`/`.format`, adjacent literals, cross-statement
  glued literals) and the guard's own tests are mutation-killable.
- **#421, #417: `405-repro.sh` / `serve-boot-check.py` self-test fixes — fleet-internal files.**
- **#423 (export hygiene):** the fourth privacy sweep excludes `docs/serve/**`, the six fleet-only
  ops scripts, `docs/measurements/**`, `docs/incidents/**` and one internal test from the export;
  `gate-env.conf.example` names every env var `gate.py` reads with its default; `STATUS-JSON.md`
  no longer points a public reader at a private doc.

### Install

- **Piped install EPIPE (#414 o):** when the piped install rig's reader stops early, a broken pipe
  is no longer the verdict — bash's own exit status and output asserts are.
- **Watcher fix (#302):** the public-release watcher waits for THIS tag's workflow run instead of
  the newest run (the wrong-run match could publish a stale release page).

### Docs

- **`docs/STATUS-JSON.md` slots row documents the #414 re-read semantics (#430).**
