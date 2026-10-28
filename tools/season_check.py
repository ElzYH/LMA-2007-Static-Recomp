#!/usr/bin/env python3
"""Check a mod space's leagues and cup finals against the live Transfermarkt data (lane 2/3 of the
2026/27 season work, #699, #607).

  tools/season_check.py <mods/NAME> <db-export dir with the base clubs.csv> <tm_live/squads.csv>

Leagues are read the way the fixtures read them, by club-id range: England 0-19 / 20-43 / 44-67 /
68-91 / 92-115 (lma-data db.rs), then each nation's ladders in #666's order and sizes (Scotland,
Spain, Italy, France, Germany, Netherlands, Portugal: 445 clubs). A club's name is the base name of
the record `record_of` moved to its id (clubs_fm.csv keeps pre-move names for some ids), or the
mod's own name for an added club. Each tier is compared with the live comp's clubs (names folded,
a few aliases); OK when the sets agree, else the clubs only on one side. Then each cup's three
latest finals (cups.csv row 0 = the latest season) with names, for a look.
"""
import collections, csv, os, re, sys, unicodedata

SIZES = [('England', [20, 24, 24, 24, 24], ['GB1', 'GB2', 'GB3', 'GB4', 'GBN']), ('Scotland', [12, 10, 10, 10], ['SC1', 'SC2', 'SC3', 'SC4']),
         ('Spain', [20, 22], ['ES1', 'ES2']), ('Italy', [20, 22], ['IT1', 'IT2']), ('France', [20, 20, 20], ['FR1', 'FR2', 'FR3']),
         ('Germany', [18, 18, 19, 18], ['L1', 'L2', 'L3', 'L3B']), ('Netherlands', [18, 20], ['NL1', 'NL2']), ('Portugal', [16, 16], ['PO1', 'PO2'])]
ALIAS = {'manutd': 'manchesterunited', 'mancity': 'manchestercity', 'spurs': 'tottenhamhotspur', 'wolves': 'wolverhamptonwanderers',
         'amadrid': 'atleticodemadrid', 'lacoruna': 'deportivoacoruna', 'parisstgermain': 'parissaintgermain', 'lilleosc': 'losclille',
         'lorientbretagne': 'fclorient', 'lemansuc72': 'lemansfc', 'stevenageborough': 'stevenagefc', 'bayernmunchen': 'bayernmunich',
         'interb': 'intermilan', 'fcdresden': 'sgdynamodresden', 'fczwolle': 'peczwolle', 'hearts': 'heartofmidlothianfc', 'sportingbraga': 'scbraga'}


def fold(s):
    return re.sub(r'[^a-z0-9]', '', unicodedata.normalize('NFKD', s).encode('ascii', 'ignore').decode().lower())


def match(a, b):
    return a == b or a in b or b in a or ALIAS.get(a) == b or ALIAS.get(b) == a


def main():
    mod_dir, base_dir, squads = sys.argv[1:4]
    base = {int(r['id']): r['name_E'] for r in csv.DictReader(open(os.path.join(base_dir, 'clubs.csv'), encoding='utf-8-sig'))}
    mod = {int(r['id']): r for r in csv.DictReader(open(os.path.join(mod_dir, 'database', 'clubs.csv'), encoding='utf-8-sig'))}

    def name(i):
        if i == '':
            return '-'
        i = int(i)
        m = mod.get(i, {})
        src = int(m['record_of']) if m.get('record_of') else i
        return m.get('name_E') or base.get(src, f'#{i}')

    live = collections.defaultdict(set)
    for r in csv.DictReader(open(squads, encoding='utf-8')):
        live[r['comp']].add(r['club'])
    start = 0
    for nation, tiers, comps in SIZES:
        for n, comp in zip(tiers, comps):
            ids = range(start, start + n)
            start += n
            m = {fold(name(i)): name(i) for i in ids}
            l = {fold(c): c for c in live.get(comp, ())}
            if not l:
                print(f'--   {nation} {comp} ids {ids.start}-{ids.stop - 1}: no live data')
                continue
            only_mod = sorted(m[a] for a in m if not any(match(a, b) for b in l))
            only_live = sorted(l[b] for b in l if not any(match(a, b) for a in m))
            print('OK  ' if not (only_mod or only_live) else 'DIFF', f'{nation} {comp} ids {ids.start}-{ids.stop - 1} ({n} slots, live {len(l)}): only mod {only_mod} | only live {only_live}')
    cups = os.path.join(mod_dir, 'database', 'cups.csv')
    if os.path.exists(cups):
        by = collections.defaultdict(list)
        for r in csv.DictReader(open(cups, encoding='utf-8-sig')):
            by[r['cup']].append(r)
        for cup, rs in by.items():
            rs.sort(key=lambda r: int(r['row']))
            print(f'{cup:15}', ' | '.join(f"r{r['row']} {name(r['winner_id'])} {r['winner_goals']}-{r['runner_up_goals']} {name(r['runner_up_id'])}" for r in rs[:3]))


if __name__ == '__main__':
    main()
