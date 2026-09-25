#!/bin/bash
# Run from any box with ssh to the GPU host (LSS_BUILD_HOST): build the static collector in the
# container on the GPU host and install/upgrade it as a systemd --user service.
# Touches nothing but its own files: ~/.local/bin/lss-collector, ~/.local/bin/lss (and their .prev
# backups - card #246), ~/bin/lss-alert.sh,
# ~/.config/lss/collector.toml (only if absent), ~/.config/systemd/user/lss-collector.service.
# An existing collector.toml is never overwritten; if it differs from the shipped example the
# difference is printed so a stale value (or a key the example gained) is seen, not guessed.
set -euo pipefail
HOST="${LSS_BUILD_HOST:?set LSS_BUILD_HOST to the ssh name of the GPU host}"
REMOTE_DIR="${LSS_BUILD_DIR:-lss-build}"
HERE="$(cd "$(dirname "$0")" && pwd)"

"$HERE/remote-build.sh" build
# shellcheck disable=SC2087  # deliberate: $REMOTE_DIR expands here, everything escaped expands on the GPU box
ssh "$HOST" bash -s <<EOT
set -euo pipefail
cd ~/$REMOTE_DIR
loginctl show-user "\$USER" -p Linger | grep -q 'Linger=yes' || { echo "lingering is off: run 'loginctl enable-linger \$USER' first" >&2; exit 1; }
mkdir -p ~/.local/bin ~/bin ~/.config/lss ~/.config/systemd/user ~/.local/state/lss
# card #246: keep what is about to be replaced, so a bad upgrade is one command from undone. <name>.prev
# is the version running right before THIS install; <name>.prev-YYYYMMDD is the one running before
# the FIRST install of that day (never overwritten by a second install the same day).
backed_up=0
for b in lss-collector lss; do
  if [ -e ~/.local/bin/\$b ]; then
    cp -p ~/.local/bin/\$b ~/.local/bin/\$b.prev
    [ -e ~/.local/bin/\$b.prev-\$(date +%Y%m%d) ] || cp -p ~/.local/bin/\$b ~/.local/bin/\$b.prev-\$(date +%Y%m%d)
    backed_up=1
  fi
done
install -m 0755 dist/lss-collector ~/.local/bin/lss-collector.new && mv -f ~/.local/bin/lss-collector.new ~/.local/bin/lss-collector
install -m 0755 dist/lss ~/.local/bin/lss
install -m 0755 scripts/lss-alert.sh ~/bin/lss-alert.sh
# card #116: install-collector.sh installed lss-collector.service and nothing schedules the
# WATCH sweep, so a fresh install read "checked never" forever. watch-check.sh is safe to run
# unconditionally now (card #116 also fixed its empty-source-list crash), so this timer ships
# alongside the collector like it does, not as a separate opt-in step.
install -m 0755 scripts/watch-check.sh ~/.local/bin/watch-check.sh
[ -e ~/.config/lss/collector.toml ] || install -m 0644 packaging/collector.toml.example ~/.config/lss/collector.toml
diff -u ~/.config/lss/collector.toml packaging/collector.toml.example || echo "NOTE: ~/.config/lss/collector.toml differs from the shipped example (above: - live, + example). Left as it is."
~/.local/bin/lss-collector --check-config >/dev/null
install -m 0644 packaging/lss-collector.service ~/.config/systemd/user/lss-collector.service
install -m 0644 packaging/lss-watch.service ~/.config/systemd/user/lss-watch.service
install -m 0644 packaging/lss-watch.timer ~/.config/systemd/user/lss-watch.timer
systemctl --user daemon-reload
systemctl --user enable lss-collector >/dev/null 2>&1
systemctl --user restart lss-collector
systemctl --user enable --now lss-watch.timer >/dev/null 2>&1
sleep 8
systemctl --user --no-pager status lss-collector | head -12
curl -fsS -m 5 http://127.0.0.1:8099/health
if [ "\$backed_up" = 1 ]; then
  echo
  echo "ROLLBACK (the version that ran before this install):"
  echo "  ssh $HOST 'cp -p ~/.local/bin/lss-collector.prev ~/.local/bin/lss-collector && cp -p ~/.local/bin/lss.prev ~/.local/bin/lss && systemctl --user restart lss-collector'"
else
  echo
  echo "first install on this host: no previous binaries, so no .prev backups and no rollback"
fi
EOT
