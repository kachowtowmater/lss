# Engines: what lss can watch, and what each one tells it

lss watches whatever serves the model on the machine. It does not need to be told which engine
that is:

```
lss-collector --detect        # finds the LLM server(s) on this machine, says why, changes nothing
```

```
Found 1 LLM server on this machine:

  Ollama at http://127.0.0.1:11434
    why      /api/version says 0.5.1 and /api/tags lists models
    model    mistral:latest  (7.2B · Q4_0 · 100% on the GPU)
    context  8192 tokens
    config   [[engine]]
             kind = "ollama"
             url = "http://127.0.0.1:11434"
```

With no config at all (`engine_url = "auto"`, the default) the collector does the same search when
it starts, and keeps looking every 15 seconds until a server answers.

## Support matrix

What each engine publishes decides which parts of the screen have numbers. A number an engine does
not publish is shown as **`n/a (not reported by <engine>)`** - never as 0 - and lss's own idle
probe (C1) still measures writing speed and time to first word on every engine.

| | SGLang | vLLM | llama.cpp server | TGI | Ollama | LM Studio | any OpenAI-compatible |
|---|---|---|---|---|---|---|---|
| found automatically | yes | yes | yes | yes | yes | yes | yes |
| model name | yes | yes | yes | yes | yes (what is loaded) | yes (what is loaded) | yes |
| UP / DOWN, incidents, alerts | yes | yes | yes | yes | yes | yes | yes |
| requests running / queued | yes | yes | yes | yes | n/a | n/a | n/a |
| how many run at once (slots) | yes | set `slots` | yes | yes | set `slots` | set `slots` | set `slots` |
| writing speed (decode) now | yes | yes (from its counters) | yes (from its counters) | yes (from its counters) | probe | probe | probe |
| reading speed (prefill) | yes | yes | yes | n/a | probe TTFT only | probe TTFT only | probe TTFT only |
| token counters (TOKENS page) | yes | yes | yes | yes | n/a | n/a | n/a |
| request counter (requests per minute) | yes | yes | n/a | yes | n/a | n/a | n/a |
| memory (KV) in use | yes | yes | n/a (removed upstream) | n/a | n/a | n/a | n/a |
| prefix-cache hits | yes | yes | yes | n/a | n/a | n/a | n/a |
| speculative decoding | yes | yes | yes | n/a | n/a | n/a | n/a |
| time to first word (histogram) | yes | yes | n/a | n/a | n/a | n/a | n/a |
| time between tokens | yes | yes | n/a | mean per request | n/a | n/a | n/a |
| request duration, queue time | yes | yes | n/a | yes | n/a | n/a | n/a |
| context length | yes | yes | yes | yes | yes | yes | n/a |
| C1 probe (speed alone, TTFT) | yes | yes | yes | yes | yes | yes | yes |
| `lss bench` (built-in mini bench) | yes | yes | yes | yes | yes | yes | yes |
| LANES / USERS / GATEWAY | only with a gateway in front (`gate_url`); without one they say "no gateway configured" | | | | | | |

"probe" = the number comes from lss's own C1 probe: one short request every 5 minutes, only when
the server is idle, over the OpenAI chat API (`/v1/chat/completions`), which every engine here
speaks (Ollama and TGI next to their native API). It is shown as what it is - "from the monitor's
own C1 test: this engine does not report a speed" - never as the engine's own number.

**What a probe can prove, per engine.** A reading only counts as "the speed one user gets on an
idle server" if nothing else ran during it. Where there is a gateway, its admitted counters
settle that. The engine's own running/queue before and after are a cheap first filter - SGLang,
vLLM, llama.cpp and TGI report those - but `running` is a gauge sampled every 5 s, so a request
that starts and finishes entirely between two scrapes is invisible to it (card #45, 2026-09-20:
three live readings passed that check with `running=0.0` throughout while another client's
request ran the whole time). What actually decides, on any engine with a generation-token
counter, is that counter: it may grow by no more than the probe's own answer across the probe's
own window, because a cumulative counter cannot miss a request the way a periodic gauge can. (A
GPU cold-clock check was tried alongside this and reverted the same day - a power-managed GPU
idling down between requests is the routine state right before almost every genuinely-idle probe,
not a rare event, and the counter check alone already caught what it was built for.) Ollama, LM
Studio and a plain OpenAI-compatible server report no running/queue count at all, so nothing can
positively prove a probe ran alone there: the reading is kept and labelled `could not rule out
other traffic`, and MODEL shows the median of many such readings rather than nothing at all. An
engine that loads a model on the first request (Ollama) is probed with the first model it lists,
so C1 learns there too.

## How each engine is recognised, and what is read

Every name below was confirmed from the project's current source or docs on 2026-09-20 (the URL
is given); the parsers are tested against real sample text in `fixtures/engines/`.

**What that sample text actually is, per engine (card #25, 2026-09-22 - RAN vs ACCEPTED ON
REPORT, said plainly rather than left to look uniform):**

| engine | verified how |
|---|---|
| SGLang | RUN live against the production serve (this project's own monitored box) |
| vLLM | **RUN**: `vllm/vllm-openai-cpu:latest` (v0.30.0), CPU-only, `facebook/opt-125m`, no host ports, no GPU, in a throwaway container. `fixtures/engines/vllm_metrics.txt` is that capture, verbatim, after 3 real `/v1/completions` requests through it |
| LM Studio | **RUN**: `llmster` 0.0.25-1 (LM Studio's own headless daemon, Linux, no display/X11), `qwen2.5-0.5b-instruct`, no host ports. `fixtures/engines/lmstudio_api_v0_models.json` / `lmstudio_v1_models.json` are that capture. **Found and fixed a real bug this run could never have found from an assembled fixture**: zero-config `--detect` silently misclassified a real LM Studio server as a generic, un-adapted "openai" server - see below |
| Apple GPU (`ioreg`) | **RUN**: `ioreg -r -d 1 -w 0 -c IOAccelerator` (no sudo) on a real Apple Silicon Mac. `fixtures/engines/ioreg_ioaccelerator.txt` is that capture. The PARSER is checked against real output; the compiled `lss-collector` binary has still never scraped it end-to-end on macOS - no macOS build exists yet (see "A macOS collector binary" below) |
| llama.cpp, Ollama | RUN (card #16's own verifier pass, CPU-only, no GPU, no published ports) |
| TGI | **ATTEMPTED, could not be safely verified.** `ghcr.io/huggingface/text-generation-inference:latest`, no `--gpus` flag, `--disable-custom-kernels` per TGI's own quicktour - it silently used real GPU memory anyway (the GPU box's docker exposes GPUs to a container even without an explicit `--gpus` flag), so it was killed within seconds; a retry with `CUDA_VISIBLE_DEVICES=""` (forcing zero visible devices) touched no GPU but failed to start at all: `RuntimeError: 0 active drivers ([]). There should only be one.` This TGI release has no genuine CPU-only serving path in this environment, not just a slow one. `fixtures/engines/tgi_metrics.txt` / `tgi_info.json` remain ASSEMBLED from the project's source, unverified against a live server |
| AMD GPU (`amd-smi`) | **NOT RUN — no AMD GPU hardware exists anywhere in this fleet to capture from.** `fixtures/engines/rocm_smi.json` and the `parse_amd_smi_json` test's inline sample remain ASSEMBLED from AMD's own documented field shapes, not a live capture. A real capture needs either physical AMD GPU hardware or a cloud AMD GPU instance - out of scope to provision without that being asked for explicitly |

### The LM Studio detection bug this run found
Zero-config `--detect`'s shared path-budget logic (`engine.rs::detect`, at most 4 real requests
per port) decided whether to spend its 4th request on Ollama's `/api/tags` or LM Studio's
`/api/v0/models` by checking `once("/api/version").is_ok()` - "did the HTTP request succeed at
all". A REAL LM Studio server answers **every** path with HTTP 200, including a JSON *error* body
for one it does not implement (`200 {"error":"Unexpected endpoint or method. (GET
/api/version)"}`) - so the budget was always spent on Ollama's check, `/api/v0/models` was never
asked, and a real LM Studio server was silently misclassified as a generic, un-adapted `openai`
server. `Ollama::detect()` itself already guards against exactly this ("`/api/version` alone is
not enough - other servers answer it"), checking for a genuine `"version"` string; the shared
path-budget decision now uses that same, stricter check. Fixed; regression test uses the real
captured response shape, not a guess at what LM Studio "should" return.

### SGLang (default port 30000)
- **Recognised by** `/metrics` carrying `sglang:` series (start it with `--enable-metrics`), else `/get_server_info`.
- **Read:** the `sglang:` gauges, counters and histograms (running / queued requests, token usage,
  `gen_throughput`, `cached_tokens_total`, `realtime_tokens_total{mode=…}`, the four latency
  histograms, speculative-decoding counters), `/get_server_info` for `max_running_requests`.
- Source: `python/sglang/srt/observability/metrics_collector.py`.

### vLLM (default port 8000)
- **Recognised by** `/metrics` carrying `vllm:` series.
- **Read:** `vllm:num_requests_running`, `vllm:num_requests_waiting`, `vllm:kv_cache_usage_perc`,
  `vllm:prompt_tokens_total`, `vllm:prompt_tokens_cached_total`, `vllm:generation_tokens_total`,
  `vllm:request_success_total{finished_reason}`, `vllm:prefix_cache_queries_total` /
  `vllm:prefix_cache_hits_total`, `vllm:num_preemptions_total`,
  `vllm:spec_decode_num_drafts_total` / `…_num_draft_tokens_total` / `…_num_accepted_tokens_total`
  (there is no acceptance gauge: lss computes accept length = 1 + accepted / drafts), and the
  histograms `vllm:time_to_first_token_seconds`, `vllm:inter_token_latency_seconds`,
  `vllm:e2e_request_latency_seconds`, `vllm:request_queue_time_seconds`,
  `vllm:request_prompt_tokens`, `vllm:request_generation_tokens`; KV capacity from the
  `vllm:cache_config_info` labels (`num_gpu_blocks` x `block_size`).
- **Renames lss reads both sides of:** `gpu_cache_usage_perc` -> `kv_cache_usage_perc` and
  `gpu_prefix_cache_*` -> `prefix_cache_*` (new names from v0.9.2, old ones removed in v0.12.0);
  `time_per_output_token_seconds` -> `inter_token_latency_seconds` (from v0.10.2, old removed in
  v0.15.0). The V1 engine has no throughput gauge: writing speed is the growth of
  `generation_tokens_total` between two scrapes.
- Not published: how many requests run at once (`--max-num-seqs`) - set `slots` in the config.
- Sources (upstream vLLM repo): `docs/usage/metrics.md`, `vllm/v1/metrics/loggers.py`, `vllm/v1/spec_decode/metrics.py`.

### llama.cpp server (`llama-server`, default port 8080)
- **Recognised by** `/v1/models` saying `"owned_by":"llamacpp"`, else `llamacpp:` series, else `/props`.
- **Read:** `/metrics` (only when started with `--metrics`): `llamacpp:prompt_tokens_total`
  (since 2025 this EXCLUDES cached tokens), `llamacpp:prompt_tokens_cached_total`,
  `llamacpp:prompt_seconds_total`, `llamacpp:tokens_predicted_total`,
  `llamacpp:tokens_predicted_seconds_total`, `llamacpp:requests_processing`,
  `llamacpp:requests_deferred`, the `llamacpp:spec_decode_*` counters. `/props` for the context
  length (`default_generation_settings.n_ctx`), `total_slots`, the model file; `/slots`
  (`is_processing`) for how many slots are busy when `--metrics` is off.
- **No longer exists upstream:** `llamacpp:kv_cache_usage_ratio` and `llamacpp:kv_cache_tokens` -
  memory (KV) use is `n/a`. There are no latency histograms either.
- Sources (upstream llama.cpp repo): `tools/server/server-task.cpp`, `tools/server/README.md`.

### Hugging Face TGI (default port 3000 from the launcher, 80 in the Docker image)
- **Recognised by** `/info` saying `"router":"text-generation-router"`, else `tgi_` series.
- **Read:** `tgi_queue_size`, `tgi_batch_current_size`, `tgi_request_success`,
  `tgi_request_duration`, `tgi_request_queue_duration`,
  `tgi_request_mean_time_per_token_duration`, `tgi_request_generated_tokens`,
  `tgi_request_input_length` (TGI's counters carry no `_total`; its token totals only exist as
  histogram sums); `/info` for `model_id`, `max_total_tokens`, `max_concurrent_requests`.
- No time-to-first-token histogram, no KV or cache numbers. The project is archived upstream
  (last release v3.3.7): the adapter is frozen at those names.
- Sources (upstream TGI repo): `router/src/server.rs`, `docs/source/reference/metrics.md`.

### Ollama (default port 11434)
- **Recognised by** `/api/version` answering AND `/api/tags` listing models.
- **Read:** `/api/ps` for what is loaded (name, size, how much of it is on the GPU,
  `context_length`), `/api/version`. Ollama has **no metrics endpoint** (checked against its whole
  route table): everything else is lss's own probe, through Ollama's OpenAI-compatible `/v1`.
  Nothing loaded is still UP ("(no model loaded)"): Ollama loads on the first request.
- Sources (upstream Ollama repo): `server/routes.go`, `api/types.go`, `docs/api.md`.

### LM Studio (default port 1234)
- **Recognised by** `/api/v0/models` (LM Studio's own API).
- **Read:** which model is `loaded` (the OpenAI list has every downloaded one), its context length,
  architecture and quantisation. No metrics endpoint.
- Sources (upstream LM Studio docs): `rest/endpoints`, `openai-compat/models`.

### Anything else that answers `/v1/models`
A gateway, a proxy, or an engine lss has no adapter for. It is watched as `openai`: UP / DOWN, the
model name, the probe, the mini bench. A plain OpenAI-compatible answer that serves the same
model as a recognised engine on the same machine is taken for a gateway in front of it, not for a
second server.

## Pinning, and several servers on one machine

`--detect` prints the block that pins what it found. A pinned engine is never searched for:

```toml
[[engine]]
kind = "vllm"                  # sglang | vllm | llamacpp | ollama | lmstudio | tgi | openai | auto
url = "http://127.0.0.1:8000"
```

One collector watches one server (its first `[[engine]]`). For a second server on the same machine
run a second collector with its own config (`lss-collector --config ~/.config/lss/collector-2.toml`)
that has its own `[[engine]]`, `listen = ["127.0.0.1:8100"]` and `db_path`, and list both in
`~/.config/lss/lss.toml` - each becomes a server on the screen (`[` and `]` switch, the FLEET line
shows all of them):

```toml
[[server]]
name = "ollama"
url = "http://127.0.0.1:8099"
[[server]]
name = "vllm"
url = "http://127.0.0.1:8100"
```

`install.sh` sets this up for every server it finds.

## A server behind https (card #308)

`url = "https://…"` works for the engine and for `gate_url`. The client is rustls, pure Rust, so
the static release binaries stay static. The certificate is checked against the bundled Mozilla
roots and against this machine's own store when it has one. So a public certificate, or one
from a CA you already installed on this machine, works with no settings.

For a box on your own network that signs its own certificate:

| collector.toml | what it does |
|---|---|
| `tls_ca_file = "~/lan-ca.pem"` | Trusts that CA. If the file is the server's **own** self-signed certificate (what `openssl req -x509` makes), it is trusted as an exact pin: that certificate and no other. |
| `tls_verify = false` | Accepts **any** certificate. At the top of the file it covers the engine and `gate_url`; in an `[[engine]]` block it covers that engine only, and the gateway keeps the top-level setting (card #316). The collector logs a warning at every start, naming each URL it applies to. Use `tls_ca_file` where you can. |

A refused certificate turns up as the serve being down. The reason is given in words: self-signed,
signed by a CA this machine does not trust, does not match the pinned one in `tls_ca_file`, expired,
not valid yet, or not issued for this name. The fixes are named in the same message.

## GPUs

| source | how | what it gives |
|---|---|---|
| NVIDIA | `nvidia-smi` | temperature, power, clock, load, memory, throttling, Xid errors, link / ECC health |
| AMD | `amd-smi metric --json`, else `rocm-smi … --json` | temperature (junction), power, load, VRAM |
| Apple Silicon | `ioreg -r -d 1 -w 0 -c IOAccelerator` (no sudo) | load and the memory the GPU holds. Temperature and power need `powermetrics`, which needs root: they read `-`, never 0 |
| none | - | GPUS leaves the screen ("GPU stats unavailable"); nothing alerts |

## No gateway, no docker

- **No gateway** (`gate_url = ""`, the default): LANES, USERS and the GATEWAY page say "no gateway
  configured", nothing is red, and the probe and the bench talk to the engine directly.
- **No docker, or an engine that is not a container** (the normal way to run Ollama, LM Studio and
  `llama-server`): the container's start time and restart count are not known, so uptime is what
  lss itself has observed, and a restart that changes nothing about the engine is not counted as a
  new run. Everything else works: the server is found by the port it listens on, and the loadout -
  what MODEL, `lss loadouts`, `lss compare` and every bench scorecard hang on - is built from what
  the engine says about itself (engine and version in place of the image tag, plus the model,
  the context length and the slots it reports). A new model, a new engine version or a new context
  length is a new loadout, exactly as a new image or a new launch flag is for a container.

## The benchmark without installing anything

`lss bench quick` wraps `llm_decode_bench.py` when `[bench] harness` points at it. With no harness
configured it runs lss's **built-in mini bench** instead: a warm-up, writing speed with 1 / 2 / 4 /
8 requests at once (capped at the slots), reading speed at two prompt sizes (every run reads new
text, so a prefix cache cannot answer for the GPU), and the sanity checks (arithmetic, a forced tool
call, JSON). Same safety as the full one: it refuses unless the server has been idle, watches every
2 seconds and aborts when anyone else's request shows up, holds a lock, and has a hard timeout.
`accuracy` needs the harness's datasets.

An engine with an `api_key` in its `[[engine]]` block (card #312): the harness gets the key on its
**standard input**, and a small wrapper passes it on as `--api-key` inside Python. The harness then
sends it as `Authorization: Bearer`, just as the collector's own requests do. The key never
appears on the command line (`ps`), is not in the environment, and goes only to the engine, never
to the gateway. Standard input also crosses an ssh or `docker exec -i` launcher. ssh does not
pass the command's words through as they are: it joins them into one line that the remote shell
splits again. So after an `ssh` launcher, lss shell-quotes every word it adds, the wrapper included.
Words made only of safe characters stay as they are, so a plain step over ssh is unchanged.

Before the first harness step, lss asks the engine itself with the same key: `GET /v1/models`, and
when that is open, a 1-token chat request, because some engines guard only their inference routes.
When the engine refuses it (HTTP 401/403), the run stops before the harness starts. The message
says which fix applies: no key is configured, or the configured one was refused. Either way it
names `api_key` in the `[[engine]]` block and never shows the key itself. This check is needed
because the real harness, when refused, often does not say so: it prints "SGLang metrics are
disabled", shows ERROR cells and exits 0. If a refusal slips past the check anyway, lss still
reads `harness.log` for an `HTTP 401`/`403` line and fails the run.

## A macOS collector binary — what it would take (card #25, 2026-09-22)

Not built tonight, by instruction; investigated instead so the actual gap is known rather than
assumed. **The gap is NOT "no Rust toolchain on a Mac"** - a real Apple Silicon Mac in this fleet
has a working `cargo`/`rustc` (checked live: 1.95.0, both). The gap is three separate, smaller
things:

1. **This fleet's own standing rule against local compiles.** The Mac that has the toolchain also
   has a build-time guard (a `cargo` wrapper) that refuses a local build by default, because a
   heavy compile has OOM'd that exact box before (a different, much larger project's build, not
   this one). It has an explicit override, deliberately not used tonight - whether the much
   lighter `lss` workspace is safe to build there routinely is a real question, but not one to
   decide unilaterally while investigating what a *release* pipeline would need anyway.
2. **Cross-compiling FROM Linux doesn't get around it.** `the GPU box` builds the Linux static
   binaries today; targeting `aarch64-apple-darwin` from a Linux box needs an Apple SDK/toolchain
   (osxcross or similar) that is not part of this project's build setup and is its own bounded
   piece of work to add - not just a `--target` flag.
3. **Distribution, past a first working build.** An unsigned macOS binary a second person runs
   will be blocked or nagged by Gatekeeper without notarization (an Apple Developer account and a
   signing step `install.sh` does not have today). `install.sh`'s existing "release binary or
   cargo build" logic would need a macOS branch either way.

**What was actually confirmed tonight, without a binary:** the Apple GPU PARSER itself (`ioreg_*` in
`crates/lss-core/src/gpu.rs`) is now checked against a real `ioreg` capture from a real Apple
Silicon Mac (see the table above) - the piece most likely to be wrong (a real device's field
values, not a guess at their shape) already has a receipt. What remains unverified is the rest of
the collector loop end-to-end on macOS (its HTTP listener, its SQLite path, `--detect`'s port
scan) - lower-risk, since none of that is macOS-specific code, but still unrun there.
