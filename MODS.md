# Building a mod

A mod is a folder under `mods/<name>/` next to `lma-app`; the game folder is never touched and the
mod is applied in memory at start (`lma-rs/crates/runtime/src/mods.rs` lists every `mod.toml` key
and every file). This page is the recipe for a season mod like `fc27finalNN`: real squads, values,
wages, funds, tables and badges from public data sources, built by `lma-cli squad-import`.

## Sources (never in the repo)

| what | file | from |
|---|---|---|
| the game's database | `work/db_base/` | `lma-cli db-export <Export.mng> work/db_base` (the disc's `Common/GameData/Misc/Export.mng`) |
| the roster: players, ratings, clubs | `work/fc27/players.csv` | the FC 27 player export |
| attributes, skin tones | `work/fm26_players_skin.csv` | an FM export through `tools/fm_skin.py` |
| staff (managers) | `work/fm26_staff.csv` | an FM staff export |
| values, birth dates, clubs, stadiums, games | `work/transfermarkt/{players,clubs,game_lineups,games}.csv` | the Kaggle Transfermarkt dataset |
| current squads | `work/tm_live/squads.csv` | `tools/tm_live.py` (the live site) |
| current managers | `lma-rs/mods/fc27/managers_current.csv` | a Wikipedia list (`club,manager`) |
| transfers the sources lack | `lma-rs/mods/fc27/transfers_override.csv` | by hand (`player,club`) |
| club budgets | `work/fc27_budgets/budgets_gbp.csv` | the FC 27 career budgets |
| badges | an FMG logo pack | `tools/badge_pack.py` |

The mapping `lma-rs/mappings/fc27.toml` names the sources' columns, the season (`season_start`,
`base_year`), the money scale (`value_unit`, `income_scale`, ...), the ladders (which divisions the
roster's leagues and the live Transfermarkt competitions fill) and the Transfermarkt competitions
that fill the histories and last season's tables (`[transfermarkt.history]`: `leagues`, `cups`,
`h2h`, `last_season`).

The game's leagues are fixed club-id blocks, so a division keeps its 2006 size whatever the real
league has today: Ligue 1, Ligue 2, Serie B, Segunda and the Eerste Divisie are bigger than their
leagues (the spare places go to clubs the sources have in no league, from the division itself first),
the Primeira Liga and Liga Portugal 2 are smaller (the real league's last clubs drop a division).
Not fixable in a mod; `tools/league_check.py` lists each block against the live leagues.

## One command

```
lma-cli db-export <Export.mng> work/db_base                 # once per game version (~5 s)
lma-cli squad-import work/db_base work/fc27/players.csv lma-rs/mappings/fc27.toml <mods dir> fc27finalNN \
  --fm work/fm26_players_skin.csv --tm work/transfermarkt/players.csv --staff work/fm26_staff.csv \
  --tm-clubs work/transfermarkt/clubs.csv --tm-lineups work/transfermarkt/game_lineups.csv \
  --tm-games work/transfermarkt/games.csv --tm-live work/tm_live/squads.csv \
  --managers lma-rs/mods/fc27/managers_current.csv --transfers lma-rs/mods/fc27/transfers_override.csv \
  --budgets work/fc27_budgets/budgets_gbp.csv \
  --from <mods dir>/fc27finalMM                                # the previous final
lma-cli db-import <Export.mng> <mods dir>/fc27finalNN/database check.mng   # must succeed
```

About 15 s. `--from` carries what the previous final keeps by hand: every `mod.toml` key the
mapping doesn't write (`wage_cap`, `subs`, `bench`, `sponsor_unit`, `sponsor_offer_scale`,
`europe_places`, `gate_scale`, ...), its `[[patch]]` tables at addresses the new file doesn't patch
(the funds fit's tier words, the board's share), and its `badges/`. The report
(`import-report.txt`) lists every match, move, takeover, fill and gap; read its last lines.

What the import writes:

| file | holds |
|---|---|
| `mod.toml` | the keys above |
| `database/players.csv`, `clubs.csv` | the records that change (names, ratings, nationalities, numbers, division moves) |
| `database/history.csv`, `cups.csv`, `last_tables.csv` | league champions, cup finals, last season's final tables (a new career's European entrants) |
| `wages.csv`, `funds.csv`, `wage_budgets.csv`, `sponsors.csv` | weekly wages, starting funds, wage budgets, shirt deals (`funds.csv` and `sponsors.csv` are hand-kept: the import doesn't write them; `--from` carries the earlier final's rows by club record, `record_of` in clubs.csv, since the re-seat moves ids; by name when a clubs.csv has no record_of) |
| `squad_overflow.csv`, `clubs_fm.csv`, `strings.csv`, `funds_targets.csv` | players beyond 32, badge keys, renamed strings, the funds fit's targets |

## Then

- **Funds to the pound** (optional): `tools/funds_fit.py` fits the game's tier words to
  `funds_targets.csv` and prints `[[patch]]` words; append them to `mod.toml` once, `--from` carries
  them from then on.
- **Badges** for new clubs: `tools/badge_pack.py <pack.zip> <mod>/clubs_fm.csv <mod>/badges`.
- **The launcher's list**: one line in `<play dir>/mods/menu.txt`, `id|label|description`, newest
  first (`squad-import --from` prints a template).
- **Check in game**: a new career at team select (funds, squads), the first inbox, a league table.
  A career started on an older final keeps that final's data: the launcher gives each version its
  own careers and quick save.

## Keys you may want to change

Everything else is `mod.toml` (see `mods.rs` for the full list): `bench = 9` and `subs = 5` for
the modern bench, `ai_wage_offers = true` so the AI pays modern wages, `ai_signing_wage_cap = 2000000` so AI clubs can sign players paid over GBP 100k a week (#704), `europe_places` for the
competitions' places, `[[patch]]` for any word in the game's image (`from` must match, so a wrong
address is refused).

**Calendar** (#703): with `base_year = 2026` alone, the disc's 2006 fixtures keep their calendar
dates, so a Saturday round lands on a Wednesday. `calendar_weekdays = true` moves every round to
its old weekday (+3 days in 2026/27, the Premier League opening Sat 15 Aug), and `calendar_weeks = 1`
a week later on top: the real 2026/27 opening days (Premier League Sat 22 Aug, EFL Sat 15 Aug).
Each season moves as one block, so no round passes another; a season whose late-June rounds can't
take the extra week keeps its weekdays only. `calendar_weeks` takes 0 or 1 (more pushes late
rounds past the season's end) and needs `calendar_weekdays`. Only a new career reads the calendar.

**Formations** (`formations = "formations.csv"`, #691, #749): `id,name,roles[,spots]`, ids 0-13
change a shape, 14 and 15 add one (the game's 4-bit field holds 16). Roles are the 11 slots as
`GK FB SW CB WB CM WM AM CF WF` (slot 1 the goalkeeper); `spots` (the pitch diagram's grid, 0-33)
can be left out for a name like `4-2-3-1`. A career saved with its team on shape 14 or 15 needs
the mod that added them: loaded without it, the match engine reads that shape past the game's
14-row tables.
