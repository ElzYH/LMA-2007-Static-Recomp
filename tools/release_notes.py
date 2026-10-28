#!/usr/bin/env python3
"""What's new in a build, from the PRs merged since the previous release (the maintainer notices what changed
in his build; until rc4 every release's notes were written by hand).

  python tools/release_notes.py v1.0-rc4 [main]       Markdown on stdout, for the release page and
                                                      ~/lma/play/VERSION
  python tools/release_notes.py v1.0-rc4 --all        also the docs, CI and agent-tooling PRs
  python tools/release_notes.py v1.0-rc4 --short      subjects over 120 characters cut at a word
  python tools/release_notes.py --selftest

It reads the squash-merge subjects (`<what> (#N)`) between the two refs and sorts them into what a
player meets (sound, the app and controls, the FC 27 data, the game itself, speed), most of it in
the order it landed. BACKLOG, HANDOFF, LESSONS, CLAUDE.md, CI and agents' tools are left out
unless --all. The subjects are the PR titles, so a good title is a good line here. An integration
PR ("... merged together", #838) becomes one line per part, from its body's `| merged in | PR | what |`
table (needs gh; without it the one subject stays).

A play build made from a branch (mac/build-1026b: other branches merged together, not squashed)
reads the same way: each `Merge ... branch 'origin/<b>'` on the first-parent line becomes the title
of <b>'s pull request, and a PR met twice (merged again after a fix, or inside an integration PR)
is listed once. Merge resolutions (`integration: ...`) are left out.
"""
import re, subprocess, sys

# first match wins; each is (heading, pattern on the lowercased subject)
SECTIONS = [
    ("Left out", r"^(backlog|handoff|lessons|claude\.md|coordination|ci:|ci |agent_inbox|tools/(build_|ci_|release_|cleanup|package_|no_game|mac_check|regress|nightly|test_)|nightly|runtime tests|squad-import test)|(^|\W)tests? only"
                 r"|lma_[a-z_]+_log=|\blogs each\b|^lma_input|^integration:|^tests:"),  # debug switches and log files: for us, not the player
    ("Speed", r"^(guest|recompiler|recomp|fence|hle|runtime \(#6)"),  # by the part they change, before the keywords
    ("Sound", r"audio|sound|\bxact|\bxma|ffmpeg|music|crowd"),
    ("The app and controls", r"launcher|lma-app|glyph|icon|font|keyboard|gamepad|\bpad\b|co-op|coop|window|fullscreen|vsync|settings|menu|bug report|f12|disc|install|first run|first-run"),
    ("FC 27 data", r"squad-import|lma-import|fc27|badge|champion|cup final|wealth|wage|values?\b|transfermarkt|fd_history|manager"),
    ("Speed", r"fence|wgpu|guest|recompiler|recomp|hle|perf|faster|memory|cpu|gpu|battery|frame time"),
    ("The game", r""),
]
ORDER = ["Sound", "The app and controls", "FC 27 data", "Speed", "The game", "Left out"]  # headings, once each


def classify(subject):
    s = subject.lower()
    for name, pat in SECTIONS:
        if not pat or re.search(pat, s):
            return name
    return "The game"


def line(subject, width=0):
    """The subject as a note line, ending in its numbers once: "x (#844) (#845)" -> "x (#844, #845)".
    With a width (--short), an overlong subject is cut at a word."""
    m = re.search(r"\s*\(#(\d+)\)\s*$", subject)
    text, pr = (subject[:m.start()], m.group(1)) if m else (subject, None)
    issue = re.search(r"\s*\((#\d+(?:, #\d+)*)\)$", text) if pr else None  # only numbers in it
    if issue:
        text, pr = text[:issue.start()], f"{issue.group(1)[1:]}, #{pr}"
    if width and len(text) > width:
        text = text[:width - 3].rsplit(" ", 1)[0] + "..."
    return f"- {text}" + (f" (#{pr})" if pr else "")


def pr_body(n):
    r = subprocess.run(["gh", "pr", "view", n, "--json", "body", "--jq", ".body"], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else ""


def expand(subject, body_of=pr_body):
    """An integration PR's subject as its parts' subjects (`<what> (#N)`), any other subject as itself."""
    m = re.search(r"\(#(\d+)\)\s*$", subject)
    if not m or "merged together" not in subject.lower():
        return [subject]
    parts = []
    for row in body_of(m.group(1)).splitlines():
        cells = [c.strip() for c in row.strip().strip("|").split("|")]
        if len(cells) >= 3 and re.match(r"#\d+", cells[-2]):
            prs = ", ".join(re.findall(r"#\d+", cells[-2].split("(")[0]))  # "#835 (= #822)": the PR merged
            parts.append(f"{cells[-1]} ({prs}, in #{m.group(1)})")
    return parts or [subject]


def branch_pr(branch):
    """'<title> (#N)' of the newest pull request from `branch`, or None (no gh, no PR)."""
    r = subprocess.run(["gh", "pr", "list", "--state", "all", "--head", branch, "--limit", "1",
                        "--json", "number,title", "--jq", '.[]|"\\(.title) (#\\(.number))"'], capture_output=True, text=True)
    return r.stdout.strip() or None if r.returncode == 0 else None


def resolve(subject, pr_of=branch_pr):
    """A merge commit's subject as its branch's PR title; any other subject as itself."""
    m = re.match(r"Merge (?:remote-tracking )?branch '(?:origin/)?([^']+)'", subject)
    if not m:
        m2 = re.match(r"Merge (\S+) \((#\d+)\) into ", subject)  # "Merge mac/battery-saver (#845) into ..."
        if not m2:
            return subject
        return pr_of(m2.group(1)) or f"{m2.group(1)} ({m2.group(2)})"
    return pr_of(m.group(1)) or f"{m.group(1)} (branch, no PR)"


def unique(subjects):
    """Each PR once (its first time), subjects without a number as they come."""
    seen, out = set(), []
    for s in subjects:
        tail = s.rsplit("(", 1)[-1] if s.rstrip().endswith(")") else ""
        prs = set(re.findall(r"#(\d+)", re.sub(r"in #\d+", "", tail)))  # "(#781, #803, in #838)": 781, 803
        if prs and prs <= seen:
            continue
        seen |= prs
        out.append(s)
    return out


def notes(subjects, include_all=False, title=None, width=0):
    groups = {name: [] for name in ORDER}
    for s in subjects:
        groups[classify(s)].append(line(s, width))
    out = [f"## {title}", ""] if title else []
    for name in ORDER:
        if groups[name] and (include_all or name != "Left out"):
            out += [f"### {name}", *groups[name], ""]
    left = len(groups["Left out"])
    if left and not include_all:
        out.append(f"(and {left} docs, CI or agent-tooling change{'s' if left != 1 else ''}; --all lists them)")
    return "\n".join(out).rstrip() + "\n"


def main(argv):
    if argv == ["--selftest"]:
        assert classify("BACKLOG: mac3's lane at the 22 Oct switch-off (#808)") == "Left out"
        assert classify("CI: release tags get their own run (#789)") == "Left out"
        assert classify("xact_cache: decode every wave bank once at install (#824)") == "Sound"
        assert classify("Keyboard button icons in game (#694 step d) (#800)") == "The app and controls"
        assert classify("squad-import: wealth tiers by rank (#809)") == "FC 27 data"
        assert classify("formations: slots 14/15 on the Team Tactics row (#749)") == "The game"
        assert classify("Fence wait (#312, #6): parks a thread (#810)") == "Speed"
        assert classify("Original game (exact) / Original game+ / modded (#821)") == "The game"
        assert classify("First-run install: the sound cache, checked headless (#840)") != "Left out"
        assert classify("guest: the flat 4 GB map with its alias on Android, xma-free (#6) (#758)") == "Speed"
        assert classify("hooks: LMA_AI_OFFER_LOG=<file> logs each AI wage offer (#823)") == "Left out"
        assert classify("LMA_INPUT: an optional :port (1-4) per item, so lma-cli can drive controllers headless (#804)") == "Left out"
        assert line("lma-app: F12 writes a bug report (#816)") == "- lma-app: F12 writes a bug report (#816)"
        assert line("x " * 100 + "(#1)", 120).endswith("... (#1)") and "..." not in line("x " * 100 + "(#1)")
        assert line("lma-app: battery saver (F2) (#844) (#845)") == "- lma-app: battery saver (F2) (#844, #845)"
        assert line("Fence wait (#312, #6) (#810)") == "- Fence wait (#312, #6, #810)"
        assert line("xact (#772 step 1) (#778)") == "- xact (#772 step 1) (#778)"
        n = notes(["BACKLOG: x (#1)", "audio: y (#2)"])
        assert "### Sound" in n and "BACKLOG" not in n and "1 docs, CI" in n, n
        n = notes(["guest: x (#3)", "perf: y (#4)"])
        assert n.count("### Speed") == 1, n
        body = "x\n| merged in | PR | what |\n|---|---|---|\n| a | #810 | fence wait as the default |\n| b, c | #781, #803 | the launcher font |\n| d | #835 (= #822) | co-op: keyboard + pad |\n"
        e = expand("the maintainer's build: the reviewed PRs merged together (#838)", lambda n: body if n == "838" else "")
        assert e == ["fence wait as the default (#810, in #838)", "the launcher font (#781, #803, in #838)",
                     "co-op: keyboard + pad (#835, in #838)"], e
        assert expand("audio: y (#2)", lambda n: body) == ["audio: y (#2)"]
        assert expand("all merged together (#9)", lambda n: "") == ["all merged together (#9)"]
        prs = {"mac/launcher-polish": "Launcher fits the window (#841)", "mac/integration-1026": "the reviewed PRs merged together (#838)"}
        assert resolve("Merge remote-tracking branch 'origin/mac/launcher-polish' into mac/build-1026b", prs.get) == "Launcher fits the window (#841)"
        assert resolve("Merge branch 'win/x' into y", prs.get) == "win/x (branch, no PR)"
        assert resolve("Merge mac/battery-saver (#845) into mac/build-1026b", prs.get) == "mac/battery-saver (#845)"
        assert resolve("audio: y (#2)", prs.get) == "audio: y (#2)"
        assert unique(["a (#810)", "b (#841)", "a again (#810, in #838)", "b (#841)", "c"]) == ["a (#810)", "b (#841)", "c"]
        assert unique(["x (#781, #803, in #838)", "font (#803)"]) == ["x (#781, #803, in #838)"]
        assert unique(["font (#803)", "disc (#781)", "both (#781, #803, in #838)"]) == ["font (#803)", "disc (#781)"]
        assert classify("integration: close the wealth-by-rank test (merge resolution)") == "Left out"
        print("selftest ok")
        return 0
    args = [a for a in argv if a not in ("--all", "--short")]
    if not args or len(args) > 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    since, until = args[0], args[1] if len(args) > 1 else "HEAD"
    r = subprocess.run(["git", "log", "--reverse", "--first-parent", "--format=%s", f"{since}..{until}"], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr.strip())
    subjects = unique([p for s in r.stdout.splitlines() if s.strip() for p in expand(resolve(s))])
    sha = subprocess.run(["git", "rev-parse", "--short", until], capture_output=True, text=True).stdout.strip()
    sys.stdout.write(notes(subjects, "--all" in argv, title=f"What's new since {since} ({len(subjects)} changes, up to {sha})", width=120 if "--short" in argv else 0))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
