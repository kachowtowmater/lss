#!/usr/bin/env python3
"""Tests for scripts/hooks/pre-push-scope delete-refspec handling (card #394).

Git's pre-push hook receives one line per pushed ref on stdin:
    <local ref> <local sha> <remote ref> <remote sha>
A branch DELETE (`git push origin --delete X`) sends the all-zeros sha as the
local ref and pushes no content, so the hook's "your branch must contain
origin/main" content check has nothing to bite on. #394: deletes must be
allowed through even from a checkout that is behind origin/main, while every
content push keeps today's refusal behavior.

Fixture (mirrors the lead's gate): a throwaway bare remote plus a clone that
falls behind origin/main, with the hook installed via core.hooksPath exactly
the way scripts/install-push-hook.sh does it.

Run:  python3 -m unittest discover -s scripts/tests -p 'test_*.py'
      (or) python3 scripts/tests/test_pre_push_scope.py
stdlib only, matching the script under test.
"""
import os
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
HOOK = os.path.join(HERE, "..", "hooks", "pre-push-scope")

ZERO40 = "0" * 40
ZERO64 = "0" * 64


def run(cmd, cwd, check=True, env=None):
    proc = subprocess.run(
        cmd, cwd=cwd, env=env,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    if check and proc.returncode != 0:
        raise AssertionError(
            "command failed: %r\nrc=%d\nstdout=%s\nstderr=%s"
            % (cmd, proc.returncode, proc.stdout.decode(), proc.stderr.decode())
        )
    return proc


class PrePushScopeDeleteTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="pre-push-scope-test-")
        self.addCleanup(self._cleanup)
        self.remote = os.path.join(self.tmp, "remote.git")
        self.hooks = os.path.join(self.tmp, "hooks")
        self.w = os.path.join(self.tmp, "w")
        run(["git", "init", "-q", "--bare", self.remote], self.tmp)
        run(["git", "clone", "-q", self.remote, self.w], self.tmp)
        run(["git", "config", "user.email", "t@example.com"], self.w)
        run(["git", "config", "user.name", "t"], self.w)
        # Install the hook the way a real clone has it: core.hooksPath holds a
        # pre-push wrapper that execs the in-repo copy of the hook.
        os.makedirs(self.hooks)
        wrapper = os.path.join(self.hooks, "pre-push")
        with open(wrapper, "w") as f:
            f.write(
                "#!/bin/bash\n"
                'exec bash "$(git rev-parse --show-toplevel)'
                '/scripts/hooks/pre-push-scope" "$@"\n'
            )
        os.chmod(wrapper, 0o755)
        os.makedirs(os.path.join(self.w, "scripts", "hooks"))
        with open(HOOK) as f:
            hook_src = f.read()
        installed = os.path.join(self.w, "scripts", "hooks", "pre-push-scope")
        with open(installed, "w") as f:
            f.write(hook_src)
        os.chmod(installed, 0o755)
        run(["git", "config", "core.hooksPath", self.hooks], self.w)
        # Base commit on main so origin/main exists for later pushes.
        with open(os.path.join(self.w, "a.txt"), "w") as f:
            f.write("a\n")
        run(["git", "add", "-A"], self.w)
        run(["git", "commit", "-qm", "base"], self.w)
        run(["git", "push", "-q", "origin", "HEAD:main"], self.w)

    def _cleanup(self):
        run(["rm", "-rf", self.tmp], self.tmp, check=False)

    def _make_other_ahead(self):
        """A second clone moves origin/main forward; the first clone stays behind."""
        other = os.path.join(self.tmp, "other")
        run(["git", "clone", "-q", self.remote, other], self.tmp)
        run(["git", "config", "user.email", "t@example.com"], other)
        run(["git", "config", "user.name", "t"], other)
        with open(os.path.join(other, "b.txt"), "w") as f:
            f.write("b\n")
        run(["git", "add", "b.txt"], other)
        run(["git", "commit", "-qm", "ahead"], other)
        run(["git", "-c", "core.hooksPath=/dev/null", "push", "-q",
             "origin", "HEAD:main"], other)

    def test_delete_allowed_from_behind_checkout(self):
        """A branch DELETE succeeds even though HEAD lacks origin/main (#394)."""
        run(["git", "checkout", "-qb", "doomed"], self.w)
        with open(os.path.join(self.w, "d.txt"), "w") as f:
            f.write("d\n")
        run(["git", "add", "d.txt"], self.w)
        run(["git", "commit", "-qm", "doomed"], self.w)
        run(["git", "push", "-q", "origin", "doomed"], self.w)
        self._make_other_ahead()
        run(["git", "fetch", "-q", "origin"], self.w)
        # HEAD (doomed) does not contain the new origin/main: before #394 the
        # hook refused every push from here. The delete must still go through.
        proc = run(["git", "push", "origin", "--delete", "doomed"],
                   self.w, check=False)
        self.assertEqual(
            proc.returncode, 0,
            "delete refused from a behind checkout:\n%s\n%s"
            % (proc.stdout.decode(), proc.stderr.decode()),
        )

    def test_delete_line_shapes(self):
        """is_delete_line matches the 40- and 64-zero local shas, nothing else."""
        # Source only the function: the hook file has no import guard and
        # sourcing it whole would run the check and exit the shell.
        script = (
            "eval \"$(sed -n '/^is_delete_line()/,/^}/p' %s)\" || exit 9; "
            "is_delete_line %s && echo a; "
            "is_delete_line %s && echo b; "
            "is_delete_line abc || echo c; "
            "is_delete_line 0000 || echo d"
            % (HOOK, ZERO40, ZERO64)
        )
        proc = run(["bash", "-c", script], self.tmp)
        self.assertEqual("a\nb\nc\nd\n", proc.stdout.decode())

    def test_stale_content_push_still_refused(self):
        """A content push that would undo origin/main keeps today's refusal."""
        self._make_other_ahead()
        run(["git", "fetch", "-q", "origin"], self.w)
        run(["git", "checkout", "-qb", "stale"], self.w)
        with open(os.path.join(self.w, "s.txt"), "w") as f:
            f.write("s\n")
        run(["git", "add", "s.txt"], self.w)
        run(["git", "commit", "-qm", "stale"], self.w)
        proc = run(["git", "push", "origin", "stale"], self.w, check=False)
        self.assertNotEqual(
            proc.returncode, 0,
            "a new branch lacking origin/main was ALLOWED - hook lost its teeth",
        )
        self.assertIn(b"REFUSING", proc.stderr)

    def test_mixed_delete_and_stale_content_refused(self):
        """A push pairing a delete with stale content is still refused."""
        run(["git", "checkout", "-qb", "doomed"], self.w)
        with open(os.path.join(self.w, "d.txt"), "w") as f:
            f.write("d\n")
        run(["git", "add", "d.txt"], self.w)
        run(["git", "commit", "-qm", "done"], self.w)
        run(["git", "push", "-q", "origin", "doomed"], self.w)
        self._make_other_ahead()
        run(["git", "fetch", "-q", "origin"], self.w)
        proc = run(
            # Colon refspec form: `:doomed` deletes, HEAD:stale2 pushes content.
            ["git", "push", "origin", ":doomed", "HEAD:stale2"],
            self.w, check=False,
        )
        self.assertNotEqual(
            proc.returncode, 0,
            "mixed delete + content push was allowed from a behind checkout",
        )
        # The content line was refused, so git reports the whole push as failed.
        self.assertIn(b"REFUSING", proc.stderr)


if __name__ == "__main__":
    unittest.main()
