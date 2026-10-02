#!/usr/bin/env bash
# The ONE definition of the paths the public export never carries (card #553, after #468's
# SKIP_PRIVACY_SCAN=1 pushed a builder past the privacy hook on docs/serve/405-repro.sh).
# scripts/export-public.sh sources this and the pre-commit privacy hook sources it too, so the
# hook now skips every path the export excludes - "what ships" and "what is scanned" can no
# longer drift apart. Before this file the hook hand-kept only three entries (gate/*, site/*,
# .privacy-words) while excluded() had grown to ~15: the hook blocked edits to excluded files
# on their pre-existing private words, and the escape hatch was the loud bypass.
#
# Sourced, not executed: it defines only excluded() and prints nothing. "$1" inside excluded()
# is a single path.
#
# never exported, whatever else happens. Keep this list and your site directory's README in step.
excluded() {
    case "$1" in
        # gate/ is the gateway engineer's: its upstream name, our public endpoint and its
        # test user names are DEPLOYMENT vocabulary, not product surface, and the words are
        # load-bearing in its tests - a public export carries the product, not our box names.
        site/ours/*|gate/*|.privacy-words) return 0 ;;
        # card #149 (PUBLIC BLOCKER) + #143: the gateway's DEPLOY tooling goes with the gateway.
        # gate/ is already excluded, so a public copy that shipped these handed a stranger a
        # script for deploying a component they do not have, plus a test binary that reads
        # a gateway source file for GATE_VERSION and therefore FAILED 8 of 10 tests on a fresh
        # export. A red suite on a first clone is the strongest signal that a project is
        # unmaintained, and it made "is main green" unverifiable by anyone outside this seat.
        # The env EXAMPLE goes too: it is the gateway config, and it is where our own host name
        # and two model ids leaked into the public tree (card #144).
        scripts/gate-deploy.sh|crates/lss-collector/tests/gate_deploy_script.rs|packaging/gate-env.conf.example) return 0 ;;
        # card #163 item 1 (verifier-2/verifier-3): same class again - this script rewrites
        # ~/.omp/agent/config.yml (omp is OUR agent-infrastructure tool, not something a stranger
        # runs) and its default GATE_URL is our trusted lane at 127.0.0.1:8096. A stranger has no
        # omp and no reason to run this.
        scripts/omp-sync-default-model.sh) return 0 ;;
        # card #180 gate 2 (from the unmerged fix/143 branch, whose exclusion half never landed -
        # only its docstring half did): this script reads the GATEWAY's admission shadow log, and
        # gate/ does not ship. A stranger gets a python script for a component they do not have,
        # pointed at a state directory that does not exist on their machine. Same call as
        # gate-deploy.sh and omp-sync-default-model.sh: the tooling goes with the thing it drives.
        scripts/shadow-analysis.py) return 0 ;;
        # card #180 gate 4, second sweep of the same class (lss-builder-2): this one is not
        # gateway tooling, it is AGENT-WORKFLOW tooling. scripts/worker-checkout.sh exists
        # because several AI workers edit one checkout at once (its own header says "two
        # builders/verifiers and an omp worker"), which is a fact about how this project is
        # DEVELOPED, not about the product a stranger installs. omp-sync-default-model.sh was
        # dropped for exactly this reason; this is its sibling, and it shipped in every export
        # until now - along with the test that drives it. Nothing in the public copy references
        # either any more (the card-#180 README rewrite removed the worker-checkout section), so
        # the exclusion leaves no dangling pointer.
        scripts/worker-checkout.sh|crates/lss-collector/tests/worker_checkout_script.rs) return 0 ;;
        # card #190 (verifier, re-checking #144 item 3): the same class a THIRD time, and this
        # one is a whole DOCUMENT rather than a script. docs/OMP-DEFAULT-MODEL.md is the
        # write-up of a 2026-09-20 incident on OUR agent fleet: which provider patterns are in
        # our omp config, what our agent work silently fell back to, and the edit we decided to
        # make on "every box that has omp". A stranger has no omp, no fleet and no stake in our
        # incident - and it is the doc that pointed hardest at scripts/omp-sync-default-model.sh,
        # excluded just above. The alert it documents (omp_default_mismatch) still ships and is
        # still documented, generically, in docs/RUNBOOK.md; what goes is our incident report.
        docs/OMP-DEFAULT-MODEL.md) return 0 ;;
        # card #302: how WE publish the public copy (our GitHub account, the release run we
        # watch). A stranger's copy is already the published thing; it has nothing to publish.
        # card #302 (--update): its test drives publish-public.sh, which the export does not carry
        scripts/publish-public.sh|crates/lss-collector/tests/publish_script.rs|docs/RELEASING.md) return 0 ;;
        # card #324: the e2e gate's RED-first evidence (scripts/e2e/RED-on-<sha>.txt) is the
        # development record of a gate failing BEFORE the work - a transcript of our runs, not
        # something a stranger runs or reads. The gate itself (run.sh, scenario.sh, ...) ships.
        scripts/e2e/RED-*.txt) return 0 ;;
        # card #423, one class sweep (the #190 precedent, FOURTH time this class has shipped):
        # docs/serve/ is the write-up of OUR serve's incidents and measurements - which model id
        # we serve on which GPU box, container and image tags we run, engine code paths at
        # specific file:line, per-card tb citations. A stranger has no such serve; every card's
        # entry names exactly the model the words list forbids. The ops scripts beside them are
        # the same class of DEPLOYMENT vocabulary:
        #   serve-boot-check.py  defaults --model/--container to the serve we run, and runs ON it
        #   batch-variance.py    probes that same serve at its loopback port, model id inline
        #   incident-timeline.py pins OUR 2026-09-25 16:18Z Xid-8 hang by timestamp
        #   per-caller-cache.py  joins OUR gate's shadow-log records on OUR caller ids
        #   agent-prompt-sizes.py same shadow-log shape, our per-caller admission records
        #   trie-error.py        same gateway shadow-log records (pinned copies on our GPU box)
        # (The last four have no model ids to neutralise and stay generic per card - but each
        # drives a component a stranger does not have, so the tooling goes with the thing it
        # drives, as gate-deploy.sh did in card #149.)
        docs/serve/*) return 0 ;;
        scripts/serve-boot-check.py|scripts/batch-variance.py|scripts/incident-timeline.py|scripts/per-caller-cache.py|scripts/agent-prompt-sizes.py|scripts/trie-error.py) return 0 ;;
        # card #423 (lead decision, same class as docs/serve just above): docs/measurements/ and
        # docs/incidents/ are the record of OUR fleet's measurements and incidents - pinned
        # snapshots on OUR serve host, OUR gate's shadow-log records, per-card tb citations, and
        # each measurement's reproduce block points at one of the ops scripts excluded above (or
        # at an already-excluded one). A stranger has nothing to reproduce them with. The
        # committed test of one of those scripts ships in scripts/tests/ and would point readers
        # at the script and at gate/ paths the export does not carry, so it goes with its script.
        docs/measurements/*|docs/incidents/*|scripts/tests/test_trie_error.py) return 0 ;;
        *) return 1 ;;
    esac
}

