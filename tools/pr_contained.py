#!/usr/bin/env python3
"""Which open PRs a branch already carries: their head merged into it changes nothing.

  python tools/pr_contained.py origin/mac/rc5-on-911          every open PR, against that ref
  python tools/pr_contained.py origin/main 900 901 902        only these PRs
  python tools/pr_contained.py --selftest

An integration PR (#911, #919) carries many others; after it merges, those close as "merged via".
For each PR, `git merge-tree --write-tree --merge-base=<its base> <ref> <head>` lays the PR's own change
(what GitHub shows: from where it left its base branch to its head) on <ref>: the same tree as
<ref> means everything the PR changes is already there (CONTAINED), a different tree means it adds
something (ADDS, with the files), and a conflict means it can't merge as it is (CONFLICT, the same).
Both the plain merge and the PR's own change are tried. When neither gives <ref>'s tree, the line also
says how many of the lines the PR adds <ref> has; all of them is LINES-IN (taken, then edited further
on <ref>: read before closing).
Checked by trees, not commits, so squashed and rebased copies count. Needs gh and git >= 2.38.
"""
import json, os, subprocess, sys, tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def git(*args, cwd=REPO, check=True):
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if check and r.returncode not in (0, 1):
        sys.exit(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r


def verdict(ref, head, base=None, cwd=REPO):
    """(CONTAINED | ADDS | CONFLICT, [files]) of the PR's own change (base..head) laid on ref."""
    extra = [f"--merge-base={base}"] if base else []
    r = git("merge-tree", "--write-tree", "--name-only", *extra, ref, head, cwd=cwd)
    lines = r.stdout.split("\n")
    if r.returncode == 1:  # conflicted: the tree, then the conflicted files, then messages
        files = [l for l in lines[1:lines.index("") if "" in lines else len(lines)] if l]
        return "CONFLICT", files
    tree = lines[0].strip()
    if tree == git("rev-parse", f"{ref}^{{tree}}", cwd=cwd).stdout.strip():
        return "CONTAINED", []
    files = git("diff", "--name-only", ref, tree, cwd=cwd).stdout.split()
    return "ADDS", files


def added_lines_in(ref, base, head, cwd=REPO):
    """(found, total) of the non-blank lines the PR adds (base..head) that ref's copy of the same
    file has: a change that ref carries but has edited further since conflicts, yet its lines are
    there (an integration branch that took a PR and then fixed on top of it)."""
    diff = git("diff", "-U0", "--no-color", base, head, cwd=cwd).stdout.split("\n")
    have = total = 0
    path, text = None, None
    for l in diff:
        if l.startswith("+++ "):
            path = l[6:] if l.startswith("+++ b/") else None
            r = git("show", f"{ref}:{path}", cwd=cwd, check=False) if path else None
            text = set(r.stdout.split("\n")) if r is not None and r.returncode == 0 else set()
        elif l.startswith("+") and path and l[1:].strip():
            total += 1
            have += l[1:] in text
    return have, total


def open_prs(numbers):
    fields = "number,title,headRefName,headRefOid,baseRefName"
    if numbers:
        return [json.loads(subprocess.run(["gh", "pr", "view", n, "--json", fields], cwd=REPO, capture_output=True, text=True, check=True).stdout) for n in numbers]
    out = subprocess.run(["gh", "pr", "list", "--state", "open", "--limit", "200", "--json", fields], cwd=REPO, capture_output=True, text=True, check=True).stdout
    return sorted(json.loads(out), key=lambda p: p["number"])


def selftest():
    with tempfile.TemporaryDirectory() as d:
        g = lambda *a: git("-c", "user.name=t", "-c", "user.email=t@t", *a, cwd=d)
        g("init", "-q", "-b", "main")
        w = lambda f, s: open(os.path.join(d, f), "w").write(s)
        w("a", "1\n"); g("add", "."); g("commit", "-qm", "base")
        g("checkout", "-qb", "pr"); w("b", "x\n"); g("add", "."); g("commit", "-qm", "pr adds b")
        g("checkout", "-qb", "other", "main"); w("a", "2\n"); g("commit", "-qam", "other edits a")
        g("checkout", "-qb", "integ", "main"); w("b", "x\n"); w("c", "y\n"); g("add", "."); g("commit", "-qm", "integration: b squashed in, plus c")
        assert verdict("integ", "pr", cwd=d) == ("CONTAINED", []), verdict("integ", "pr", cwd=d)
        assert verdict("main", "pr", cwd=d) == ("ADDS", ["b"])
        g("checkout", "-qb", "clash", "main"); w("a", "3\n"); g("commit", "-qam", "clash on a")
        assert verdict("other", "clash", cwd=d) == ("CONFLICT", ["a"]), verdict("other", "clash", cwd=d)
        # a PR on an old base whose own change is in: a plain merge conflicts, its own diff doesn't
        g("checkout", "-qb", "late", "other"); w("d", "z\n"); g("add", "."); g("commit", "-qm", "late adds d")
        g("checkout", "-qb", "integ2", "main"); w("a", "9\n"); w("d", "z\n"); g("add", "."); g("commit", "-qm", "a moved on, d in")
        assert verdict("integ2", "late", cwd=d)[0] == "CONFLICT"
        assert verdict("integ2", "late", base="other", cwd=d) == ("CONTAINED", [])
        # taken, then edited further on the integration branch: every added line is still there
        g("checkout", "-qb", "feat", "main"); w("e", "one\ntwo\n"); g("add", "."); g("commit", "-qm", "feat")
        g("checkout", "-qb", "integ3", "main"); w("e", "one\ntwo\nthree\n"); g("add", "."); g("commit", "-qm", "feat + a fix")
        assert added_lines_in("integ3", "main", "feat", cwd=d) == (2, 2)
        assert added_lines_in("main", "main", "feat", cwd=d) == (0, 2)
    print("selftest ok")


def main(argv):
    if argv == ["--selftest"]:
        selftest()
        return 0
    if not argv:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    ref, numbers = argv[0], argv[1:]
    git("fetch", "-q", "origin")
    for p in open_prs(numbers):
        head = p["headRefOid"]
        if git("cat-file", "-e", head, check=False).returncode:
            git("fetch", "-q", "origin", p["headRefName"])
        base = git("merge-base", f"origin/{p['baseRefName']}", head, check=False).stdout.strip() or None
        v, files = verdict(ref, head)
        if v != "CONTAINED" and base:  # the PR's own change, laid on ref
            v2, files2 = verdict(ref, head, base)
            if v2 == "CONTAINED" or v == "CONFLICT":
                v, files = v2, files2
        cover = ""
        if v != "CONTAINED" and base:
            have, total = added_lines_in(ref, base, head)
            if total:
                cover = f" [{100 * have // total}% of its {total} added lines in {ref}]"
                if have == total:
                    v = "LINES-IN"
        more = f": {', '.join(files[:4])}{' ...' if len(files) > 4 else ''}" if files else ""
        print(f"#{p['number']:<5} {v:<9} {p['title'][:60]}{cover}{more}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
