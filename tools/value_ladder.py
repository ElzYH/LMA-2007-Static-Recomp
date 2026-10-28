#!/usr/bin/env python3
"""Scale the game's player values through their ladder in a mod space (the maintainer, 11 Oct: Bellingham at GBP 32m).

    tools/value_ladder.py <mods/NAME/mod.toml> <factor>      values x factor, e.g. 3 or 2.5
    tools/value_ladder.py --selftest

`f821208e0` turns a player's overall (clamped 25..100) into a value by interpolating 22 words at
0x82005508..0x8200555c (287,500 .. 40,000); `f8220df78` scales that by the position table and the
age ramp and caps it at the stored u16 (GBP 65.5m). The ladder is the DIVISOR (value = 100 x base /
ladder: the PC measured x3 words -> values / 3), so values x factor means 22 `[[patch]]` entries with
to = round(word / factor), replacing any earlier ladder patches in the file. Other patches (funds
tiers, country multipliers) are kept.
"""
import re
import sys

LADDER_BASE = 0x82005508
LADDER = [287500, 285000, 280000, 275000, 270000, 260000, 250000, 240000, 225000, 210000, 190000,
          170000, 140000, 110000, 105000, 100000, 95000, 80000, 71000, 70000, 55000, 40000]
PATCH = re.compile(r'\n*\[\[patch\]\]\s*\naddr\s*=\s*"(0x[0-9a-fA-F]+)"\s*\nfrom\s*=\s*(\d+)\s*\nto\s*=\s*(\d+)\s*\n?')


def scaled(toml: str, factor: float) -> str:
    """`toml` with the ladder patches set to `factor`; other patches and text untouched."""
    ladder = {LADDER_BASE + 4 * i for i in range(len(LADDER))}
    out = PATCH.sub(lambda m: m.group(0) if int(m.group(1), 16) not in ladder else "\n", toml).rstrip("\n") + "\n"
    out += f"\n# Player values x{factor:g} (tools/value_ladder.py): the 22-word ladder f821208e0 interpolates by overall\n"
    out += "# divides them (0x82005508..0x8200555c, words / factor); the shown value stays capped by the stored u16 (GBP 65.5m).\n"
    for i, w in enumerate(LADDER):
        out += f'\n[[patch]]\naddr = "0x{LADDER_BASE + 4 * i:x}"\nfrom = {w}\nto = {round(w / factor)}\n'
    return out


def selftest() -> None:
    base = 'name = "x"\n\n[[patch]]\naddr = "0x82703118"\nfrom = 300\nto = 2000\n'
    once = scaled(base, 3)
    assert once.count("[[patch]]") == 23 and 'from = 287500\nto = 95833' in once and "0x82703118" in once
    twice = scaled(once, 2)
    assert twice.count("[[patch]]") == 23 and 'from = 287500\nto = 143750' in twice and "95833" not in twice
    assert twice.startswith(base.rstrip("\n")), "text before the ladder is kept"
    print("ok")


def main() -> int:
    if sys.argv[1:] == ["--selftest"]:
        selftest()
        return 0
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    path, factor = sys.argv[1], float(sys.argv[2])
    with open(path, encoding="utf-8", newline="") as f:
        toml = f.read()
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(scaled(toml, factor))
    print(f"{path}: value ladder x{factor:g} ({len(LADDER)} patches)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
