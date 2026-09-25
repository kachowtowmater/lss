# After every model swap

Seven steps, about ten minutes. The point is to end with a decision you can defend: keep the new
model, or go back, with the numbers side by side.

```
bring the serve up  ->  wait for ready  ->  re-point pinned clients  ->  lss bench quick
                    ->  lss compare current previous  ->  public smoke test  ->  decide
```

## 1. Bring the serve up
Start the new model the way you always do. `lss` does not start, stop or configure the model
server, ever.

The collector notices by itself: a changed model, image, launch flag or serving-related
environment variable is a **new loadout**, and it starts a fresh scorecard for it. The same
configuration started again is the same loadout (its restart is counted).

## 2. Wait for ready
```
lss status          # SERVE ... UP, the new model id in the first line
lss model           # "loadout <id> · first seen <now>"
```
A model load takes minutes; `serve_down` may warn and then recover on its own. Wait until
`lss status` shows `UP` and `running 0`.

## 3. Re-point anything that pins the model id
A swap usually changes the **served model id**, and every client that names a model explicitly -
a config file, an agent harness, a script, a saved request - is now naming one this server no
longer serves. What happens next depends on the client, and the dangerous case is not the loud
one:

* it errors → you find out immediately, which is the good outcome;
* it falls back to **another provider** and keeps working → nobody notices. Agent work can run
  for days on hardware you do not control, at someone else's cost and privacy terms, because
  "it kept working". This has happened; a hard failure would have been fixed in minutes.

So make the second case impossible before it can happen: on any client that can fall back to an
outside provider, **remove the outside provider from the list it is allowed to resolve against**,
so an unresolvable pinned model errors rather than silently going elsewhere. That is a decision
about the *default*, not a ban - a provider named deliberately, by hand, in the moment, is a
choice someone made and can see.

The one source of truth for the new id is the server's own `/v1/models` - the same answer the
public smoke test in step 6 reads:
```
curl -s http://<your-server>/v1/models          # the id clients must use now
```
Then update each client that pins it, and check each one actually came back.

**The collector also watches one such client for you** (`[rules] omp_config_path`, default
`~/.omp/agent/config.yml` - the agent harness `omp`, if you run it; empty means not watched): a
`modelRoles.default` that does not match what is served, held for `omp_mismatch_hold_secs`
(default 10 min), raises `omp_default_mismatch` through the same alert pipeline as everything
else - so a swap that forgets this step, on the box the collector runs on, is still caught. See
*omp_default_mismatch* in `docs/RUNBOOK.md`.

## 4. `lss bench quick`   (about 5 minutes)
```
lss bench quick --note "why this swap"
```
On the GPU box. From another machine `lss bench` prints the exact `ssh` command (or runs it, with
`bench_ssh` set in `~/.config/lss/lss.toml`). On the MODEL page the key is `b`.

What it does: a warm-up (the first prefill on a fresh container is a cold-cache artifact, so it
is never the one measured), writing speed for 1, 2, 4 and 8 users at once with an empty and a
16k-token conversation, reading speed for 8k and 64k prompts, and three sanity checks (an
arithmetic answer, a forced tool call, JSON that parses). A run that finished with a failed
check does not read as a bare OK: the scorecard's first line says `OK, BUT 1 CHECK FAILED: json
output`, and `lss` and the MODEL page's BENCH box say `ok, but 1 check failed: json output` in the
warning colour (cards #334, #338).

It protects whoever is using the server:
* it **will not start** unless the server has been idle for 5 minutes. If someone is on it,
  try again later. `--force` overrides that; think twice;
* it **stops by itself** the moment anyone else sends a request, and records
  `aborted: real traffic`. An aborted run never becomes the scorecard: run it again when it is quiet;
* alerts about queueing and the idle-speed (C1) rule stand down while it runs, so nobody is
  paged about load that is ours.

`lss bench status` shows a run in progress and the last ones; `lss bench cancel` stops one.

## 5. `lss compare current previous`
```
lss compare current previous
```
Side by side, with the difference in percent. **Within +-3% is noise** (run-to-run variation):
it is marked `~ same (noise)` and should not drive a decision. Clear wins and losses are marked.
It ends with one plain sentence per category:

| category | what it means for you |
|---|---|
| speed alone | how fast an answer is written when one person uses it |
| speed under load | how much everybody gets together, and each of them, when 8 use it at once |
| reading speed | how long a long prompt waits before the first word |
| long context | 128k prompts, a 16k conversation, the needle test (`lss bench full`) |
| accuracy | the score on a pinned question set (`lss bench accuracy`) |
| efficiency | tokens per joule: the electricity per answer |

`lss compare current best` compares with the fastest loadout ever measured here;
`lss loadouts` lists them all; on the MODEL page `enter` opens the same comparison.

## 6. The public smoke test
A swap can change the served model id, and a client pinned to the old id may break or may
silently get the new model. From OUTSIDE your network, with a real key, through the public
endpoint:
```
curl -s https://<your-endpoint>/v1/models -H "Authorization: Bearer $KEY"          # the id clients must use now
curl -s https://<your-endpoint>/v1/chat/completions -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"<the id from above>","messages":[{"role":"user","content":"Say OK"}],"max_tokens":8}'
```
Tell the people who use it what the id is now. `lss users` shows whether their requests are
coming back (`ok`) or failing (`errors`, `rejected`).

## 7. Decide
* every category `same` or better: keep it.
* faster alone but slower under load (or the other way round): what matters is how YOUR server is
  used. `lss advice` and the USERS page say how many people are on it at once.
* worse where it matters: go back to the previous loadout. Its scorecard stays on record.

## Later, in a quiet window
```
lss bench full        # ~30 min: + 128k prompts, the needle at ~250k tokens, acceptance, KV, tokens per joule
lss bench accuracy    # hours: gsm8k (or --dataset mmlu-pro | gpqa-diamond)
lss compare current previous --accuracy     # the harness's own paired test on the same questions
```
**Accuracy runs are long** (every question of the set is one request) and they load the server
the whole time. Start them when nobody needs it: late evening, a weekend. They stop by
themselves if someone does show up, and then have to be started again.

## Reading the numbers honestly
* Speculative-decoding acceptance and KV capacity are read from the **server's** metrics, never
  from the client.
* Compare like with like: `lss compare` only sets a bench number against a bench number and a
  live number against a live number, and says which (`[bench]` / `[live]`).
* One run is one run. If a result surprises you, run `lss bench quick` again before you believe it.
