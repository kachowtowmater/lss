# LLM SERVER STATUS (`lss`)

A terminal monitor for **your own LLM server**. One small collector runs on the machine that
serves the model; `lss`, a full-screen terminal view plus a plain-text / JSON CLI, runs on any
machine that can reach it. No Grafana, no Prometheus, no browser, no account, no cloud.

It answers, in plain words:

* **What is worst right now, and what does it cost me?** One line at the top names the current
  bottleneck - a hot GPU, a full KV cache, requests held at a gateway, an engine that stopped
  producing tokens - and says what it means for you, not just which number moved.
* **Is it up, and how is it serving?** time to first token (p50/p90/p99), reading (prefill) and
  writing (decode) speed, what is running and waiting, KV memory, speculative acceptance, cache hits.
* **What is it costing me?** your GPUs' power priced against **your** electricity plan, right now
  and for today, yesterday, this week and this month, and the cost per million tokens of real work.
* **Who is using it, and how much?** tokens served by hour / day / week / month, and per user
  when a gateway in front of the engine publishes them.
* **How good is THIS model on THIS hardware?** speed at 1, 2, 4, 8 users at once, reading speed,
  memory, energy per million tokens, a repeatable benchmark per model, and a side-by-side
  comparison with the model before.
* **What went wrong, and when?** alerts that fire and clear, and a dated record of incidents:
  outages, restarts, GPU Xid errors.

## What it needs

* A machine serving a model with **SGLang, vLLM, llama.cpp server, Ollama, LM Studio, TGI, or
  anything OpenAI-compatible**. `lss-collector --detect` finds it; you do not have to say which.
* **Linux or macOS.** NVIDIA GPUs are read through NVML (the driver's own library, falling back
  to `nvidia-smi`); AMD through `amd-smi` / `rocm-smi`; Apple silicon through `ioreg`. No GPU tool
  at all is fine - the GPU parts say so.
* Docker is optional (it adds container restarts and the exact launch settings of each model).
* A gateway in front of the engine is optional (it adds lanes, per-user numbers and the
  gateway's own counters).
* **Rust only if there is no release binary for your machine** - see Install.

Nothing talks to the internet on its own. The one exception is the cost wizard's IP lookup, which
makes a single HTTPS request and only when you say yes. The only request lss ever sends your
model is a tiny idle test (below), and a benchmark only when you ask for one.

## Install

One line, on the machine that serves the model (or any machine that can reach it):

```
curl -fsSL https://github.com/kachowtowmater/lss/releases/latest/download/install.sh | bash
lss
```

Or from a clone:

```
git clone https://github.com/kachowtowmater/lss && cd lss
./install.sh               # asks before each step
./install.sh --dry-run     # prints every step it would take, changes nothing
./install.sh --yes         # accepts every default, asks nothing (CI, scripts)
```

Piped from `curl`, the installer reads its questions from your terminal (`/dev/tty`), never from
the pipe, so the one-liner is just as interactive as `./install.sh`. Every option also works
piped: `curl -fsSL .../install.sh | bash -s -- --yes`.

The installer does three things, and nothing needs root:

1. **Gets the programs.** `lss` (the screen) and `lss-collector` (the background part) go into
   `~/.local/bin`. It downloads the release built for your machine and checks it against the
   release's published `.sha256` file before installing it. From a clone with no matching release
   it builds with Rust. With no release and no Rust it stops, installs nothing, and says where to
   get Rust (<https://rustup.rs>). It never leaves a half-install.
2. **Runs the setup wizard** (`lss setup`, below): finds your LLM server, tests it live, sets
   the electricity rate, writes `~/.config/lss/`.
3. **Runs the collector in the background,** then prints the one command to type: `lss`.
   - **macOS:** a launchd agent (`ai.lss.collector`). It starts at login and restarts by itself.
   - **Linux with a systemd user session** (a normal login, or ssh): a systemd `--user` unit,
     `lss-collector`. It starts at login and restarts by itself. `sudo loginctl enable-linger
     $USER` keeps it running while you are logged out.
   - **Linux without one** (a container, WSL, a shell reached with `su` or `sudo -u`): the
     installer says `no systemd --user session and no launchd here (a container?)` and starts
     the collector with `nohup` instead. Its process id goes in `~/.local/state/lss/lss-collector.pid`
     and its log in `~/.local/state/lss/lss-collector.log`. **It does not come back after a
     reboot.** To start it again, run the command the installer printed (it writes the log and a
     fresh pid file, as the installer did):
     `nohup ~/.local/bin/lss-collector --config ~/.config/lss/collector.toml >>~/.local/state/lss/lss-collector.log 2>&1 & echo $! > ~/.local/state/lss/lss-collector.pid`.
     To get the systemd unit instead on a machine that has systemd:
     1. Run `sudo loginctl enable-linger $USER`.
     2. Log in as that user directly (ssh, not `su`), or `export
        XDG_RUNTIME_DIR=/run/user/$(id -u)`. `systemctl --user` needs one of these.
     3. Run the installer again. It first stops the `nohup` collector it started before (the
        one in the pid file, and only if that process really is `lss-collector`), then starts the
        unit. It keeps your settings and asks before replacing any file.

     In a container, start the collector from the container's own start command instead.

Installer options: `--prefix DIR`, `--version vX.Y.Z`, `--repo OWNER/NAME` (or `$LSS_REPO`),
`--binary-dir DIR` (install programs you already have), `--no-build`, `--no-service`, and every
setup-wizard flag below. `./install.sh --help` lists them all.

What each engine publishes, and so which numbers read `n/a (not reported by <engine>)` instead of
a made-up zero, is in **[docs/ENGINES.md](docs/ENGINES.md)**.

## Setup wizard (`lss setup`)

The installer runs it for you. Run it again any time, for example to point lss at another engine:

```
lss setup
```

A real run, answered at a terminal (the e2e gate on macOS at commit dd18a2b, against a stand-in
vLLM server). Steps 1-4 are shown. Two edits: the throwaway home directory is shortened to `~`,
and the test driver's own notes (the `WIZARD-ANSWER` lines that log each answer it typed) and
one blank line are left out. Every other line is as printed:

```
== 1/5  Finding your LLM server
   asking the usual ports on this machine (SGLang, vLLM, llama.cpp, Ollama, LM Studio, TGI, any OpenAI-compatible server) ...
   1) vLLM at http://127.0.0.1:18431 - e2e-fake-vllm  [sure: /metrics carries `vllm:` series]
   Which one should lss watch? (1, o = another address, q = quit) [1]

== 2/5  Testing http://127.0.0.1:18431
   [ok] models: e2e-fake-vllm
   [ok] engine: vLLM (/metrics carries `vllm:` series)
   [ok] metrics: vLLM publishes 5 of the 16 numbers lss shows (the rest read 'n/a' - docs/ENGINES.md says which and why)

== 3/5  Electricity cost (optional)

Electricity cost - how should lss price the power your GPUs draw?
  1) my ZIP code       - my state's average home rate (U.S. EIA, 2026-06); nothing is sent anywhere
  2) look it up        - from my internet address, via ONE request to ipinfo.io (asks first)
  3) type my own rate  - $ per kWh from my electricity bill (the most accurate)
  4) skip              - no cost figures for now
Choose 1-4 [1]

Your 5-digit ZIP code:
94103
ZIP 94103 is in California: average home rate 34.74c/kWh (U.S. EIA, 2026-06, published August 26, 2026).
Use it? [Y/n]

rate: CA avg (EIA 2026-06) = $0.3474 /kWh -> ~/.config/lss/rates.toml
restart the collector to use it (it reads rates.toml at start).

== 4/5  The settings in ~/.config/lss
   wrote ~/.config/lss/collector.toml
   wrote ~/.config/lss/lss.toml
```

It goes through these steps:

1. **Find.** It looks for SGLang, vLLM, llama.cpp, Ollama, LM Studio, TGI or anything
   OpenAI-compatible on this machine, and shows what answered and how sure it is. Found nothing,
   or the engine is on another machine? Type its address (`localhost:8000`, `192.0.2.20:8000`,
   `http://host:port` or `https://host:port`) and, if the engine was started with one, its API key.
2. **Test, live.** It asks the engine for its models and its metrics, and says in plain words what
   is wrong if something is: nothing listening on that port, a host name the network does not
   know, a missing or wrong API key, a server still loading its model. It also says which metrics
   you will get. For example, SGLang publishes them only with `--enable-metrics` and llama.cpp
   only with `--metrics`, while Ollama and LM Studio have no metrics endpoint at all (that is
   normal: lss reads what they do publish). An `https://` engine whose certificate this machine
   does not trust (usual for a box that made its own) gets two choices: **[c]** give the path of
   its certificate or of the CA that signed it (saved as `tls_ca_file`), or **[i]** do not check
   its certificate (`tls_verify = false` for that engine, with a warning at every start).
3. **Electricity cost.** This is the cost wizard, below.
4. **Write the configs**: `~/.config/lss/collector.toml` and `lss.toml`. It never replaces a file
   without asking, and it keeps a `.bak` copy when you say yes. A re-run changes only the engine
   part of an existing `collector.toml`.
5. **Restart the background collector**, then tell you to type `lss`.

Without questions (all optional; the installer accepts the same flags):

```
lss setup --url 192.0.2.20:8000 --kind vllm --api-key "$KEY" --zip 94103 --yes
```

`--url ADDR` skips the scan · `--kind sglang|vllm|llamacpp|ollama|lmstudio|tgi|openai|auto` ·
`--api-key KEY` (or `$LSS_API_KEY`) · `--all` watches every engine found (one collector each) ·
`--zip ZIP | --rate USD_PER_KWH | --from-ip | --skip-cost` answers the cost step · `--yes` asks
nothing · `--force` replaces existing configs (keeping a `.bak`) · `--no-service` leaves the
background collector alone. Exit codes: 0 done, 1 could not write, or a `--zip`/`--rate`/`--from-ip`
you gave was refused or failed (e.g. `--rate 2.5`: cents or dollars? - setup stops at the cost step
and writes nothing; pass `--rate 2.5c` or `--rate 0.31`, or `--skip-cost`), 2 bad arguments, 3 you
stopped it (nothing written).

## Point it at YOUR machine

**Which collector `lss` reads:** `--url`, else `$LSS_URL`, else `url` in `~/.config/lss/lss.toml`
([example](packaging/lss.toml.example)), else `http://127.0.0.1:8099`. To read the collector from
another machine, add the GPU box's LAN or VPN address to `listen` in `~/.config/lss/collector.toml`
and restart it.

**Which engine the collector watches:** nothing to do by default (`engine_url = "auto"` finds it
and keeps looking until one answers). To pin it, add an `[[engine]]` block - `lss-collector
--detect` prints the exact block to paste.

**Several servers:** one `[[server]] name, url` entry per collector in `lss.toml`. `[` and `]`
switch between them, a FLEET line shows every one, and `f` opens a page with one row per server.

## Set up YOUR electricity rates

Cost is **off until you give it a rate**. lss never guesses one. The setup wizard asks, and you can
re-run just this step with `lss-collector cost-setup`. There are four ways to answer:

| choice | what it does | network |
|---|---|---|
| **your ZIP code** (`--zip 94103`) | your state's average residential rate, from a table built into the program: the U.S. EIA's *Electric Power Monthly*, dated in the file it writes | none |
| **look it up from my IP** (`--from-ip`) | finds your state from this machine's public IP, then uses the same table. Only asked with your consent, and the flag IS the consent | **one** HTTPS request to ipinfo.io |
| **type your rate** (`--rate 0.31`) | your own $/kWh from your bill, the most accurate choice. `31c` and `31` also read as cents | none |
| **skip** (`--skip`) | writes nothing; cost stays off | none |

With `--zip 94103` and no questions, the same step printed this in the gate's run:

```
ZIP 94103 -> California: 34.74c/kWh average residential (EIA, 2026-06)
rate: CA avg (EIA 2026-06) = $0.3474 /kWh -> ~/.config/lss/rates.toml
restart the collector to use it (it reads rates.toml at start).
```

A ZIP or IP answer is an average for the state, not your tariff. The file it writes,
`~/.config/lss/rates.toml`, says where the rate came from and how old it is, and lss shows that
source next to the cost. It never replaces an existing `rates.toml` without asking (`--force`
does; `--dry-run` prints the file instead of writing it). Exit codes: 0 written or skipped,
1 lookup failed (a ZIP it does not cover, outside the US, no network) or `--rate` refused (a bare
small number like `2.5` could be cents or dollars: pass `2.5c` or `0.025`), 2 bad arguments,
3 an existing file was kept.

**Time-of-use, or your exact tariff:** write the file yourself.

```
cp packaging/rates.toml.example ~/.config/lss/rates.toml
$EDITOR ~/.config/lss/rates.toml
```

The [example](packaging/rates.toml.example) holds two shapes, both with obviously fake numbers:

* **flat** - one $/kWh at every hour;
* **time-of-use** - rates by season, weekday vs weekend/holiday, and hour, the way most utility
  tariffs actually bill. Each 5-second power sample is priced at the rate in force at ITS OWN
  time, so a 4-9pm peak is never averaged away.

Copy the numbers from your own bill or your utility's published tariff, and set `effective_date`
to the tariff's own date: it is printed next to every dollar figure, so a table that has gone
stale stays visible instead of being trusted forever. A daily fixed charge you set is shown for
context and **never** added into a per-hour or per-token figure. An amount you are not sure
applies (a line item that may already be inside the headline rate) goes in
`unresolved_usd_per_kwh` and is shown as a stated uncertainty rather than silently counted.

What you then see: the rate right now and its period, dollars per hour at the current draw,
today and the last 24 h on page 1, today / yesterday / this week / this month in `lss status`
(each saying how much of its window it actually covers), a 30-day daily chart on the TOKENS page, and **cost per million tokens of real
work** - the energy divided by the tokens the engine actually computed (uncached prompt tokens +
generated tokens), with idle standby power reported separately so a quiet day does not make a
token look expensive. Every figure says what it excludes: this is the electricity your GPUs drew
at your tariff, not your whole bill, not the rest of the machine, not the hardware.

## Set up YOUR sources (the WATCH page)

The WATCH page (`w`) shows what the projects you follow for serving recipes have published
lately - engine releases, model repositories, recipe repos - with a NEW mark on anything since
you last looked. **lss never fetches anything to build it.** You choose the sources and the
schedule; lss only reads the result back.

```
cp packaging/watch-sources.txt.example ~/.config/lss/watch-sources.txt   # one source per line: name|what it feeds|owner/repo
$EDITOR ~/.config/lss/watch-sources.txt
scripts/watch-check.sh                                                   # writes ~/.config/lss/watch.json
```

Run `scripts/watch-check.sh` on whatever schedule you like (a systemd timer is in
[packaging/lss-watch.timer](packaging/lss-watch.timer)), or write `watch.json` with anything you
prefer - its shape is in [packaging/watch.json.example](packaging/watch.json.example). A summary
with nothing behind it (no release, commit or issue link) is marked **UNVERIFIED**, never shown
as a finding. No file = the page says tracking is off.

## Alerts (optional)

The collector runs any program you name as `alert_cmd <severity> <message>` (`info`, `warn`,
`page`, `hardware`). The wizard installs `lss-notify.sh` and points `alert_cmd` at it; tell it
where to deliver in `~/.config/lss/alert.env` ([example](packaging/alert.env.example)):

* `LSS_NOTIFY_NTFY=https://ntfy.sh/<your-private-topic>` - a push to your phone, no account;
* `LSS_NOTIFY_WEBHOOK=<url>` - any HTTP endpoint, POSTed `{"severity":…,"message":…}`;
* `LSS_NOTIFY_CMD="notify-send LLM-SERVER"` - any program you already have.

With none set it shows a desktop notification where it can and otherwise says plainly that nobody
is listening. Prove the path end to end: `lss-collector --test-alert "install check"`, then
`lss alerts`. `LSS_NOTIFY_DRY_RUN=1 ~/.local/bin/lss-notify.sh warn test` prints what it would send.

A machine that is down cannot report that it is down, so `scripts/install-upcheck.sh` installs a
small check on a **second** Mac that asks the GPU box every 60 s. Name what it should ask in
`~/.config/lss/upcheck.env`: `LSS_UPCHECK_SERVE_URL` (your engine's own port, or your gateway)
and `LSS_UPCHECK_COLLECTOR_URL`. The serve URL has no default on purpose - no port is right for
every engine, and a wrong guess would report "down" forever over a healthy server.

## Uninstall

```
./install.sh --uninstall
curl -fsSL https://github.com/kachowtowmater/lss/releases/latest/download/install.sh | bash -s -- --uninstall
```

Either one asks first, then stops and removes the background service(s) and the programs
(`lss`, `lss-collector`, `lss-notify.sh`) from `~/.local/bin`. Your configs in `~/.config/lss/`
and your history in `~/.local/state/lss/` stay. Delete those two directories yourself to remove
everything.

## Troubleshooting

| you see | what to do |
|---|---|
| `lss` shows the collector as not answering | Is it running? Check the way the installer started it (step 3 of Install), then `curl http://127.0.0.1:8099/health`. **Linux, systemd unit:** `systemctl --user status lss-collector`; log: `journalctl --user -u lss-collector`. **Linux, `nohup`** (the installer said `no systemd --user session`): `kill -0 $(cat ~/.local/state/lss/lss-collector.pid) && echo running`; log: `~/.local/state/lss/lss-collector.log`; stop: `kill $(cat ~/.local/state/lss/lss-collector.pid)`; start: `nohup ~/.local/bin/lss-collector --config ~/.config/lss/collector.toml >>~/.local/state/lss/lss-collector.log 2>&1 & echo $! > ~/.local/state/lss/lss-collector.pid`. Running `lss setup` again also restarts it. **macOS:** `launchctl list \| grep ai.lss.collector`; log: `~/.local/state/lss/`. |
| the collector is gone after a reboot (Linux) | It was started with `nohup` (no systemd user session at install time). Start it as above, or get a systemd unit: see step 3 of Install. |
| the wizard found no engine | Start the engine first, or give its address: `lss setup --url localhost:8000`. `lss-collector --detect` prints what it found on this machine, and why. |
| "refused the connection" for an engine on another machine | The engine is listening only on its own 127.0.0.1. Start it with `--host 0.0.0.0` (llama.cpp, vLLM, SGLang), or for Ollama set `OLLAMA_HOST=0.0.0.0`. |
| "wants an API key" / "refused the API key" (HTTP 401/403) | Re-run `lss setup` and type the key the engine was started with (`--api-key`, or `OPENAI_API_KEY`). |
| many numbers read `n/a (not reported by …)` | That engine does not publish them, or needs a flag: SGLang `--enable-metrics`, llama.cpp `--metrics`. [docs/ENGINES.md](docs/ENGINES.md) lists each engine. |
| no dollar figures | No rate is set yet: `lss-collector cost-setup`. |
| stops working after you log out (Linux, systemd unit) | `sudo loginctl enable-linger $USER` |
| `note: ~/.local/bin is not on your PATH` | Add the `export PATH=...` line the installer printed to your shell profile, or type `~/.local/bin/lss`. |
| `the download does NOT match its published checksum ... - not installed` | The download was damaged or changed on the way. Nothing was installed. Run the installer again; if it happens every time, install from a clone (`./install.sh`, which builds with Rust). |
| the certificate of an `https://` engine is not accepted | Re-run `lss setup` and choose **[c]** (its certificate or CA file) or **[i]** (do not check it). Details: [docs/ENGINES.md](docs/ENGINES.md). |
| `lss` from another machine cannot connect | Add that machine's reachable address to `listen` in the GPU box's `~/.config/lss/collector.toml`, restart the collector, and give `lss` the same address with `--url`. |

What each alert on the screen means, and what to do about it: [docs/RUNBOOK.md](docs/RUNBOOK.md).

## What it does NOT do

* It **never changes your server.** No restarts, reloads, config edits or model swaps. It reads.
* It **does not route or limit traffic.** That is a gateway's job; lss only reads one if you run it.
* It **does not send your data anywhere.** No telemetry, no cloud, no account. The collector
  listens on 127.0.0.1 unless you add an address.
* It **does not load your model with its own traffic.** The only request it sends on its own is
  the idle test: one short streamed completion every 5 minutes, only while nothing else is
  running. A test that overlapped real traffic is stored as invalid and kept out of every baseline
  and alert. The test and the benchmark are never counted as users.
* It **does not benchmark unless you ask** (`lss bench`), and a benchmark refuses to start
  unless the server has been idle, and stops the moment anyone else uses it.
* It **does not guess your electricity price, your model's speed, or anything else it cannot
  measure** - see below.
* It is **not a full bill.** Cost is the GPUs' measured power at your tariff; the rest of the
  machine, cooling, fixed charges and hardware are not in it.

## The honesty rules

These hold on every screen and in `lss status`, and tests pin them:

* **A number that cannot be computed is an em dash with the reason** - `— no priced samples yet
  today`, `— not enough rollup history yet` - never a `0` standing in for "unknown", and never a
  confident sentence over a missing value.
* **A number the engine does not publish says so:** `n/a (not reported by <engine>)`.
* **No cost figure without its rate's effective date and what it excludes.**
* **A window that is only partly covered says how much** (a 3-day-old month says it is 3 days),
  instead of presenting a partial number as the full one.
* **Totals never hide a missing server:** across several servers, one the screen cannot reach
  drops out of every total and the totals line says how many were not counted - it never counts
  as a zero.
* **One severity scale.** The "worst now" line and the per-GPU boxes rank problems on one scale
  with one set of words (`watch`, `high`, `critical`, `offline`); a dead source always outranks a
  merely busy one; a fact about the setup (cost, model id, flags) is shown but can never be the
  headline.
* **Red means a problem and nothing else.** A calm server has no red on screen.

## The screen

Press **`v`** for page 1: the "worst now" line, then ALERTS and INCIDENTS, LOADOUT, ELECTRICITY
and COST PER 1M TOKENS on the left, and SERVE, LANES, TOKENS, USERS and one box per GPU on the
right (one column on a narrow pane; ALERTS/INCIDENTS lead the column at a tall enough pane so
they need no scrolling - a short, wide one keeps LOADOUT+ELECTRICITY first instead, since there
isn't room for both pairs). Every box shares one label column; a long value wraps under its own
column rather than being cut off; the page scrolls, and the footer says how much is below
(`1-20 of 63`). A GPU titled `GPU0*` is one you listed in `thermal_exclude`: its heat is still
shown and still counted in the digest, it just never raises a thermal alert and never becomes the
"worst now" line. `v` again returns to the classic overview (SERVE, GPUS, LANES, USERS, ADVICE,
INCIDENTS, ALERTS as a grid that adapts to the pane, `L` pins a layout).

| keys | |
|---|---|
| `v` | page 1 / the classic overview |
| `tab` / `shift`+`tab` | step through the overview and all ten pages, wraps - from page 1, the classic overview, or any page |
| `1` … `9` `a` | jump straight to a page (`a` is ADVICE, the tenth) · `esc` or `0` back to the overview |
| `←` `→` | a page: same as tab / shift-tab · classic overview: move focus (see below) |
| arrows, `enter` | classic overview: move focus between boxes, `enter` opens the focused one full screen |
| `↑` `↓` PgUp PgDn Home End | scroll |
| `r` | time range 15m → 1h → 6h → 24h → 7d |
| `c` | charts as lines / blocks |
| `w` · `f` | WATCH page · FLEET page (with more than one server) |
| `[` `]` | previous / next server |
| `s` | USERS: cycle the sort |
| `b` | MODEL: benchmark this model (asks first; from another machine it shows the ssh command) |
| `T` · `?` · `q` | theme · help · quit |

The screen remembers theme, pinned layout and range in `~/.config/lss/ui.json` (`$LSS_PREFS`
names another file). If the collector cannot be reached the screen says so, with the last-seen
time, and keeps showing the last data it had.

| page | what it answers |
|---|---|
| **1 LATENCY** | time to first token, end-to-end, inter-token and queue time: p50/p90/p99 over time |
| **2 LOAD** | decode and prompt tok/s, running and queued, KV use, cache hit rate, speculative acceptance, the idle-test history |
| **3 GPUS** | per-GPU temperature, power, clock, utilisation, memory, throttle reasons with Xid errors, link / ECC health |
| **4 USERS** | per user: lane, running now, requests and tokens, refusals, last seen (needs a gateway that publishes per-user numbers; it says so otherwise) |
| **5 TOKENS** | exact tokens today / 1h / 24h / 7d / all time, tokens per hour, length distributions, and the 30-day spending chart |
| **6 MODEL** | this model's scorecard on this hardware, the benchmark box, the comparison with the previous and the best model |
| **7 GATEWAY** | per-lane and per-key requests and status codes, in-flight tokens (with a gateway) |
| **8 ALERTS** | every rule with its state and threshold, alert history with delivery status |
| **9 INCIDENTS** | dated record of what happened - restarts, outages, Xid errors, maintenance, uptime % |
| **a ADVICE** | one plain sentence per finding about settings and hardware - `fine` / `watch` / `act` ([docs/ADVICE.md](docs/ADVICE.md)) |

## The CLI

```
lss                      live screen (in a terminal) · plain status once (when piped)
lss status               one-shot status, including cost          lss incidents | alerts | probe
lss users                who is on the server, requests and tokens per user
lss tokens               tokens served: today / 1h / 24h / 7d / all time
lss model                how good this model is here: speed by users at once, memory, energy
lss loadouts             every model + settings ever served, with its headline numbers
lss compare A B          two loadouts side by side (current | previous | best | id | model name)
lss bench quick|full|accuracy|dry-run|status|cancel
lss advice               evidence for settings and upgrade decisions (--range 24h|7d|30d)
lss latency|load|gpus|gateway [--range 15m|1h|6h|24h|7d]      lss rules
lss maintenance start "reason" [--minutes N] | stop | status   planned work: its restart
                                                                 shows "planned", not a warning
lss --demo               the live screen on built-in sample data (no collector needed)
exit codes: 0 serve up · 1 serve DOWN / refused · 2 collector unreachable / bad usage
```
Every command except the bare `lss` is safe for scripts, cron and agents: plain text with stable
labels, or `--json` = the collector's own documents, whose every key is documented in
[docs/STATUS-JSON.md](docs/STATUS-JSON.md).

## Loadouts, `lss bench`, `lss compare`

A **loadout** is one model served one way: model id, container image and a hash of the launch
settings, read from `docker inspect` whenever the serving container starts. Restarting the same
configuration is the same loadout; any changed setting starts a new one. Launch settings are
hashed, never stored. Every speed, memory and energy figure is kept per loadout.

`lss bench [quick|full|accuracy] [--force] [--note "…"]` runs on the GPU box; from another machine
it prints the exact ssh command (or runs it when `bench_ssh` is set in `lss.toml`). It wraps
[llm_decode_bench.py](https://github.com/local-inference-lab/llm-inference-bench) when you point
`[bench] harness` at a clone of it; without it, every profile except `accuracy` runs a small
built-in bench instead.

| profile | about | what |
|---|---|---|
| `quick` | 5 min | decode at 1, 2, 4, 8 users (capped at your slots) x short and 16k context · prefill 8k + 64k · three sanity checks |
| `full` | 30 min | `quick` with longer cells + prefill 128k + retrieval from a ~250k-token context + acceptance, KV capacity and tokens per joule from the server |
| `accuracy` | hours | a pinned dataset (`gsm8k`, `mmlu-pro`, `gpqa-diamond`); `lss compare A B --accuracy` runs a paired significance test |
| `dry-run` | seconds | proves the setup without sending the model a token |

It refuses to start unless the server has been idle for 5 minutes (`--force` overrides), checks
every 2 seconds and aborts the moment anyone else sends a request, pauses the idle test and the
queue alerts while it runs (outages, restarts and heat still alert), runs one at a time with a
hard timeout, and never lets the harness update itself.

`lss compare <A> <B>` prints two loadouts side by side with the change in %: within ±3% is
`~ same (noise)`, and it ends with one plain verdict per category. A benchmark number is only
compared with a benchmark number, a live number with a live number. The routine after swapping
models: [docs/MODEL-SWAP.md](docs/MODEL-SWAP.md).

## With a gateway (optional)

A gateway is a proxy between your users and the engine - API keys, separate lanes for outside
users and your own tools, admission control. lss does not ship one and does not need one. If
yours publishes a health document in the shape described in
[docs/STATUS-JSON.md](docs/STATUS-JSON.md) (`gate_url` in `collector.toml`), the LANES box, the
USERS and GATEWAY pages and the per-user numbers fill in; without it they say there is no
gateway, and everything else works the same.

## Config reference

### `~/.config/lss/collector.toml` (the GPU box)
Optional: [packaging/collector.toml.example](packaging/collector.toml.example) lists every key with
its default (a test holds that file to the built-in defaults). Unknown keys are an error.
`lss-collector --check-config` prints the effective config. Restart the service after editing.

| key | default | |
|---|---|---|
| `host` | `""` = this machine's name | the name shown on screen |
| `poll_secs` | `5` | |
| `engine_url`, `engine_kind` | `"auto"`, `"auto"` | the LLM server; `auto` finds it ([docs/ENGINES.md](docs/ENGINES.md)) |
| `[[engine]] kind, url, name` | none | pins what `--detect` would find |
| `serve_container`, `serve_port` | `auto`, `0` | `auto` = whichever container publishes the engine's port (survives a model swap). No docker = no container facts |
| `slots` | `0` | `0` = ask the engine (SGLang, llama.cpp and TGI say); set it for the others |
| `listen` | `["127.0.0.1:8099"]` | add a LAN / VPN address to read it from another machine |
| `db_path` | `~/.local/state/lss/lss.db` | |
| `raw_hours`, `retention_days`, `rollup_10m_days` | `24`, `14`, `90` | what each storage tier keeps |
| `alert_cmd`, `alert_flush_secs` | set by the wizard, `300` | run as `alert_cmd <severity> <message>` |
| `[rates] path` | `~/.config/lss/rates.toml` | your electricity rate table; `""` = cost off |
| `[watch] path` | `~/.config/lss/watch.json` | the WATCH page's file; `""` = off |
| `gate_url`, `gate_container` | `""` | your gateway, if you run one |
| `public_priority` / `trusted_priority` | `"10"` / `"0"` | the priority labels your gateway stamps on each lane, if it does |
| `temp_units` | `"both"` | temperatures in alert text: `both`, `c`, `f` |
| `[targets] ttft_p95_s, min_tok_s_per_user, max_queue_wait_s, uptime_pct` | `5.0, 30.0, 2.0, 99.0` | the service you want; the screen and ADVICE say how much of the last 24 h met each; `0` switches one off |
| `[[user_alias]] ip, name` | none | friendly names for users seen only by address |
| `[probe] enabled, interval_secs, max_tokens, timeout_secs, prompt` | `true, 300, 128, 60, …` | the idle test |
| `[rules] …` | see the example | alert thresholds; `thermal_exclude` lists GPUs that never raise a heat alert |
| `[bench] …` | see the example | harness path, idle gate, durations, timeouts |
| `[advice] …` | [docs/ADVICE.md](docs/ADVICE.md) | every ADVICE threshold |

### `~/.config/lss/lss.toml` (wherever you run `lss`)
| key | default | |
|---|---|---|
| `url` | `http://127.0.0.1:8099` | the collector; `$LSS_URL` and `--url` win |
| `theme` | `""` | `dark` / `light` for a first start |
| `temp_units` | both | `both`, `c`, `f` |
| `bench_ssh` | `""` | the ssh host `lss bench` runs on; empty = print the command |
| `[[server]] name, url, bench_ssh` | none | one entry per collector; `lss --server NAME …` picks one |

## Storage

One SQLite file (WAL), bounded by construction. Measured with 4 GPUs and ~90 metrics:

| tier | per day | kept | steady state |
|---|---|---|---|
| raw 5 s samples | 43 MB | 24 h | ~43 MB |
| 1-minute rollups | 3.5 MB | 14 d | ~50 MB |
| 10-minute rollups | 0.35 MB | 90 d | ~32 MB |
| latency / size histograms | <= 0.7 MB | 14 d / 90 d | <= 15 MB |
| incidents, alerts, tests, loadouts, scorecards | < 0.1 MB | 90 d | < 10 MB |

The collector itself stays around 15 MB of memory; its service is capped at 512 MB. A database from
an older collector is migrated in place; an older collector ignores newer tables, so rolling back
is starting the old binary.

## Develop

```
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
LSS_BUILD_HOST=<host> scripts/remote-build.sh test|clippy|build|bless    # the same on a remote docker host
LSS_BUILD_HOST=<host> scripts/remote-build.sh test --ref <sha>          # a pinned commit; cleans up after itself (--keep keeps it)
cargo test -p lss --test render -- --ignored --nocapture print_renders  # every layout and page as text
lss --demo --no-save                                                     # the screen on sample data
```

**Anything that drives the live screen for a test uses `--no-save` or `LSS_PREFS=$(mktemp)`**, or
it overwrites the person's saved theme and layout.

**Keep your own deployment out of the repository.** Real configs, host names and notes go under
`site/<name>/`, which is git-ignored whole; start from `packaging/*.example`. Before your first
commit run `scripts/install-privacy-hook.sh` (blocks a commit that carries a host name, a real
address, an e-mail, a credential, a value from your own rates file, or any word in your
git-ignored `.privacy-words`) and `scripts/install-push-hook.sh` (refuses a push from a branch that does not
contain its remote, which is how someone else's work gets silently undone). To produce a copy you
could publish: `scripts/export-public.sh <new-dir>` writes the tree without `site/` and runs the
same privacy check on the RESULT, deleting it on any hit.

CI (`.github/workflows/ci.yml`) runs clippy, the tests and `shellcheck`, then exports a copy
(which runs the privacy scan on it) and runs the whole test suite again inside that copy. Parsers
are tested against real captures ([what is real, what is synthetic](fixtures/README.md)); rules,
incidents, rollups and the token ledger run under a fake clock; `/status` and the other documents
have byte-for-byte goldens; the benchmark runner is tested end to end against a fake harness; the
screen and every page render in memory at the pane sizes people use.
