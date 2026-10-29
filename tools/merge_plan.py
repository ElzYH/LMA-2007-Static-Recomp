#!/usr/bin/env python3
"""Rehearse a squash-merge order before anyone merges: every PR, in the order given, squashed into
its base branch as GitHub would, with `git merge-tree` (nothing is merged, pushed or checked out).

  python tools/merge_plan.py 920 925 919 916 911 914 909 907     the PRs, in merge order
  python tools/merge_plan.py 911 919:main                         919 retargeted to main first
  python tools/merge_plan.py --selftest

Each step merges the PR's head into its base branch's tip as the rehearsal has it so far: a PR
merged into another PR's branch earlier in the list (a stack folded in) is part of that branch from
then on, and a squash makes a new commit on the base whose only parent is the old tip, as GitHub's
does. So the rehearsal finds what goes wrong only in sequence: 29 Oct, squashing #911 into main first
left #919 (on #911's branch) conflicting on main; folding #919 into #911's branch first was clean.
Prints per step `ok` or `CONFLICT` with the files, then each branch's change against origin.
A PR whose base is gone from origin can't be rehearsed (it says so).
"""
import json, os, subprocess, sys, tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def git(*args, cwd=REPO, ok=(0,)):
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if r.returncode not in ok:
        sys.exit(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r


def pr_info(n):
    out = subprocess.run(["gh", "pr", "view", str(n), "--json", "number,title,baseRefName,headRefName,headRefOid,state"],
                         cwd=REPO, capture_output=True, text=True, check=True).stdout
    return json.loads(out)


def rehearse(prs, remote="origin", cwd=REPO):
    """[(number, base, 'ok' | 'CONFLICT' | 'SKIP', files)] for prs (dicts with number, baseRefName,
    headRefName, headRefOid) squashed in order; branch tips live in a dict as the steps go."""
    tips, out = {}, []

    def tip(branch):
        if branch not in tips:
            r = git("rev-parse", "--verify", "-q", f"{remote}/{branch}", cwd=cwd, ok=(0, 1))
            tips[branch] = r.stdout.strip() or None
        return tips[branch]

    for p in prs:
        base = p["baseRefName"]
        onto = tip(base)
        # the head as the rehearsal has it: a PR branch others were folded into carries them
        head = tips.get(p["headRefName"]) or p["headRefOid"]
        if onto is None:
            out.append((p["number"], base, "SKIP", [f"{remote}/{base} doesn't exist"]))
            continue
        r = git("merge-tree", "--write-tree", "--name-only", onto, head, cwd=cwd, ok=(0, 1))
        lines = r.stdout.split("\n")
        if r.returncode == 1:
            files = [l for l in lines[1:lines.index("") if "" in lines else len(lines)] if l]
            out.append((p["number"], base, "CONFLICT", files))
            continue
        tips[base] = git("commit-tree", lines[0].strip(), "-p", onto, "-m", f"squash #{p['number']}", cwd=cwd).stdout.strip()
        out.append((p["number"], base, "ok", []))
    return out, tips


def selftest():
    with tempfile.TemporaryDirectory() as d:
        g = lambda *a: git("-c", "user.name=t", "-c", "user.email=t@t", *a, cwd=d)
        w = lambda f, s: open(os.path.join(d, f), "w").write(s)
        g("init", "-q", "-b", "main")
        w("a", "1\n2\n3\n"); g("add", "."); g("commit", "-qm", "base")
        # the integration PR (A, into main) and a PR stacked on it (B, into A's branch), both adding
        # at the end of the file as #911 and #919 did to LESSONS.md
        g("checkout", "-qb", "integ"); w("a", "1\n2\n3\n4\n"); g("commit", "-qam", "integ adds 4")
        g("checkout", "-qb", "stack"); w("a", "1\n2\n3\n4\n5\n"); g("commit", "-qam", "stack adds 5")
        rev = lambda r: g("rev-parse", r).stdout.strip()
        for b in ("main", "integ", "stack"):
            g("update-ref", f"refs/remotes/origin/{b}", rev(b))
        A = {"number": 1, "baseRefName": "main", "headRefName": "integ", "headRefOid": rev("integ")}
        B = {"number": 2, "baseRefName": "integ", "headRefName": "stack", "headRefOid": rev("stack")}
        # squash A first, then B retargeted to main: B's base is cut off, line 3 conflicts
        Bmain = dict(B, baseRefName="main")
        steps, _ = rehearse([A, Bmain], cwd=d)
        assert [s[2] for s in steps] == ["ok", "CONFLICT"] and steps[1][3] == ["a"], steps
        # fold B into A's branch first, then squash A once: clean, and main gets both edits
        steps, tips = rehearse([B, A], cwd=d)
        assert [s[2] for s in steps] == ["ok", "ok"], steps
        assert git("show", f"{tips['main']}:a", cwd=d).stdout == "1\n2\n3\n4\n5\n"
        # a base that's gone
        steps, _ = rehearse([dict(A, baseRefName="gone")], cwd=d)
        assert steps[0][2] == "SKIP"
    print("selftest ok")


def main(argv):
    if argv == ["--selftest"]:
        selftest()
        return 0
    if not argv:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    git("fetch", "-q", "origin")
    prs = []
    for a in argv:  # N or N:<base>, the PR retargeted to <base> before its merge
        n, _, base = a.lstrip("#").partition(":")
        p = pr_info(n)
        if base:
            p["baseRefName"] = base
        prs.append(p)
    for p in prs:
        if git("cat-file", "-e", p["headRefOid"], ok=(0, 1, 128)).returncode:
            git("fetch", "-q", "origin", p["headRefName"])
    steps, tips = rehearse(prs)
    bad = 0
    for (n, base, v, files), p in zip(steps, prs):
        more = f": {', '.join(files[:5])}" if files else ""
        print(f"#{n:<5} -> {base:<28} {v}{more}   {p['title'][:50]}")
        bad += v != "ok"
    for b, t in sorted(tips.items()):
        if t and t != git("rev-parse", "-q", "--verify", f"origin/{b}", ok=(0, 1)).stdout.strip():
            stat = git("diff", "--shortstat", f"origin/{b}", t).stdout.strip()
            print(f"{b}: {stat or 'no change'}")
    print("every step clean" if not bad else f"{bad} step(s) not clean")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
