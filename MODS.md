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
the modern bench, `ai_wage_offers = true` so the AI pays modern wages, `ai_signing_wage_cap = 2000000` so AI clubs can sign players paid over GBP 100k a week (#704), `rating_wage_scale = 5.0` so new and youth players get modern wages (and the demands and AI offers made from ratings scale with them, #704; pair it with `ai_wage_offers`, or wages stop at the game's GBP 250k), `europe_places` for the
competitions' places, `[[patch]]` for any word in the game's image (`from` must match, so a wrong
address is refused).

**Bench** (`bench = 6..9`, #530): the matchday squad keeps the game's 5 bench slots; the extra
players (the reserves, pre-match entries 16 and up) get rows of their own on the match screens'
team and substitutes menus, after the bench and any injured players. A reserve can be queued, cancelled
and brought on like a bench player: he takes a bench slot (one whose player already went off, else
the last), and the player he replaces moves to the reserves. The dugout lists every substitution.
Known gap: the position label of the last reserve row reads "-". `subs = 5` sets how many of them
a team may make.

**`[tune]`: the game's numbers by name** (#870). Each is one instruction or data word in the game; the
loader checks the range and that the game's own word is there before changing it. Run
`lma-cli tune-list <default.xex>` for this table with your copy's values (the tables' numbers too).

```toml
[tune]
prime_wage_premium = 15                 # one number
asking_price_top_factor = 1.5           # a float
demand_status_pct = [6, 5, 4, 3, 2, 1, 0, 1, 2, 3, 4, 5]     # a table: all its numbers (made up)
```

A table is set whole: every number, in the game's order (`lma-cli tune-list <default.xex>` prints
your copy's, ready to copy and change). Two kinds need care:
- `league_prize_territory*`: one row per final league position, one column per division
  (`[1st div1, 1st div2, ..., 2nd div1, ...]`).
- `cup_prize_*`: rows of 5, `[round, win, loss, draw, both clubs]`. Keep each row's first number:
  it is the round the row pays for, not money. "Loss" is what the beaten club gets (in a final, the
  runner-up), "both clubs" is paid to each side whatever the result.
- `development_by_age`: pairs `[growth, decline %]`, one per age band (under 21, 21-25, 26-30,
  31-33, 34+; move the bands with `development_band1_age`..`development_band4_age`). The game goes
  through a player's skills during the season: a skill grows by 1 with a chance of growth x his
  potential / 100 (more when the skill is far below what his potential and age allow; band factors
  1.25, 1.1, 1, 0.75, 0.5), and drops by 1 with a chance of decline %. Lower the late bands'
  decline to keep veterans sharp for longer.
- `young_match_growth`: rows `[from, to, bonus]` by match rating (the game rates 30..100): after
  each match a player under 24 (`young_match_growth_age`) moves each skill towards his potential by
  up to bonus hundredths of a point (all of it when the skill is 30 or more below his potential,
  less when it is closer). Keep the ranges covering 0..100.

| name | where | game's value | range | what it does |
|---|---|---|---|---|
| `prime_wage_premium` | `0x821883d8` | 35 | 0..=100 | the % a player aged 25-30 adds to his wage demand (vanilla 35) |
| `prime_wage_age_after` | `0x82188390` | 24 | 0..=60 | players older than this pay the prime wage premium (vanilla 24) |
| `prime_wage_age_before` | `0x821883a0` | 31 | 0..=60 | players younger than this pay the prime wage premium (vanilla 31) |
| `wage_refuse_margin` | `0x82188354` | 100 | 0..=32767 | a player refuses when his demand is more than this above the offered wage (vanilla 100) |
| `demand_tier_pct` | `0x8200b208`, 30 words | a table of 30 | -100..=200 | the % on his wage demand by buyer tier minus his club's tier, -15..14 |
| `sale_min_squad` | `0x82188a64` (+1 more) | 16 | 0..=31 | a club won't sell while it has this many players or fewer (vanilla 16) |
| `sale_min_available` | `0x82188a7c` (+1 more) | 11 | 0..=31 | a club won't sell while it has this many available players or fewer (vanilla 11) |
| `squad_max` | `0x82188b98` | 32 | 1..=32 | a club with this many players can't sign another (vanilla 32, the squad array's size) |
| `squad_fill_base` | `0x8218c5fc` | 18 | 0..=28 | the AI buys until it has this + its tier / 4 players (vanilla 18) |
| `bid_pct_top_low` | `0x8218914c` | 160 | 1..=400 | the AI's first bid for a status-5 player: the % range's low end (vanilla 160) |
| `bid_pct_top_high` | `0x82189148` | 170 | 1..=400 | ... its high end (vanilla 170) |
| `bid_pct_low` | `0x82189158` | 110 | 1..=400 | the AI's first bid: the % range's low end (vanilla 110) |
| `bid_pct_high` | `0x82189154` | 120 | 1..=400 | ... its high end (vanilla 120) |
| `bid_raise_pct_low` | `0x82189160` | 105 | 1..=400 | the AI's raised bid: the % range's low end (vanilla 105) |
| `bid_raise_pct_high` | `0x82189164` | 115 | 1..=400 | ... its high end (vanilla 115) |
| `bid_pct_status3_low` | `0x82189108` | 80 | 1..=400 | the AI's first bid for a status-3 player: the % range's low end (vanilla 80) |
| `bid_pct_status3_high` | `0x8218910c` | 100 | 1..=400 | ... its high end (vanilla 100) |
| `asking_price_top_factor` | `0x8200b2c8` (float) | a float | 0.5..=10 | a status-5 player's asking price: at least this x his valuation (vanilla 1.8) |
| `deal_instalments_factor` | `0x8200ae94` (float) | a float | -2..=0 | a deal's instalments count at minus this of their total (vanilla -0.9: 90%) |
| `exchange_value_status5` | `0x8200ae84` (float) | a float | 0..=5 | an exchange player of status 5 counts at this x his valuation (vanilla 1.5) |
| `exchange_value_status1_3` | `0x8200ae80` (float) | a float | 0..=5 | an exchange player of status 1 or 3: this x his valuation (vanilla 0.8) |
| `exchange_value_status2_4_6` | `0x8200ae7c` (float) | a float | 0..=5 | an exchange player of status 2, 4 or 6: this x his valuation (vanilla 0) |
| `exchange_value_other` | `0x8200ae88` (float) | a float | 0..=5 | an exchange player of any other status: this x his valuation (vanilla 0.75) |
| `funds_tier` | `0x82703118`, 16 words | a table of 16 | 0..=1000000 | the board's funds figure by wealth tier 0-15 (x 10 x the nation's byte) |
| `funds_nation_factor` | `0x820033ac`, 52 bytes | a table of 52 | 0..=255 | the board's funds figure: the factor by the club's nation group 0-51 (51 when unknown), x the tier word x 10 |
| `funds_chelsea_factor` | `0x820eab38` | 200 | 0..=30000 | the club with uid 0x1716 (Chelsea) uses this instead of its nation's byte (vanilla 200) |
| `funds_cup_club_factor` | `0x820eab18` | 100 | 0..=30000 | a club of country type 0xe in competition 0xb/0xc uses this instead of its nation's byte (vanilla 100) |
| `board_verdict_levels_short` | `0x822bd71c` | 2 | 0..=5 | the board's worst verdict (0x13) needs the club this many levels short of its target and in debt past half its funds figure (vanilla 2) |
| `league_prize_territory0` | `0x82004288`, 120 words | a table of 120 | 0..=100000000 | league prize money, territory 0: 24 positions x 5 divisions, position-major |
| `league_prize_territory1` | `0x82004468`, 48 words | a table of 48 | 0..=100000000 | league prize money, territory 1: 12 positions x 4 divisions |
| `league_prize_territory4` | `0x82004668`, 80 words | a table of 80 | 0..=100000000 | league prize money, territory 4: 20 positions x 4 divisions |
| `league_prize_territory5` | `0x82004528`, 40 words | a table of 40 | 0..=100000000 | league prize money, territory 5: 20 positions x 2 divisions |
| `league_prize_territory6` | `0x820045c8`, 40 words | a table of 40 | 0..=100000000 | league prize money, territory 6: 20 positions x 2 divisions |
| `cup_prize_22` | `0x820036a0`, 70 words | a table of 70 | 0..=100000000 | cup prize money, competition 0x22: 14 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_23` | `0x820037e0`, 55 words | a table of 55 | 0..=100000000 | cup prize money, competition 0x23: 11 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_28` | `0x820038d0`, 75 words | a table of 75 | 0..=100000000 | cup prize money, competition 0x28: 15 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_29` | `0x82003a10`, 75 words | a table of 75 | 0..=100000000 | cup prize money, competition 0x29: 15 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_2a` | `0x82003ed8`, 70 words | a table of 70 | 0..=100000000 | cup prize money, competition 0x2a: 14 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_2b` | `0x82004004`, 5 words | a table of 5 | 0..=100000000 | cup prize money, competition 0x2b: 1 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_2c` | `0x82004030`, 35 words | a table of 35 | 0..=100000000 | cup prize money, competition 0x2c: 7 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_2d` | `0x820040d0`, 5 words | a table of 5 | 0..=100000000 | cup prize money, competition 0x2d: 1 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_2e` | `0x82003b50`, 55 words | a table of 55 | 0..=100000000 | cup prize money, competition 0x2e: 11 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_2f` | `0x82003c40`, 60 words | a table of 60 | 0..=100000000 | cup prize money, competition 0x2f: 12 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_30` | `0x82003d44`, 5 words | a table of 5 | 0..=100000000 | cup prize money, competition 0x30: 1 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_31` | `0x82003d70`, 55 words | a table of 55 | 0..=100000000 | cup prize money, competition 0x31: 11 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_32` | `0x82003e60`, 25 words | a table of 25 | 0..=100000000 | cup prize money, competition 0x32: 5 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_33` | `0x82004238`, 5 words | a table of 5 | 0..=100000000 | cup prize money, competition 0x33: 1 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_34` | `0x82004260`, 5 words | a table of 5 | 0..=100000000 | cup prize money, competition 0x34: 1 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_37` | `0x82004198`, 25 words | a table of 25 | 0..=100000000 | cup prize money, competition 0x37: 5 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_38` | `0x820040f8`, 35 words | a table of 35 | 0..=100000000 | cup prize money, competition 0x38: 7 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_39` | `0x82004210`, 5 words | a table of 5 | 0..=100000000 | cup prize money, competition 0x39: 1 rows of [round, win, loss, draw, both clubs] |
| `cup_prize_2f_division_15_fee` | `0x820ecbf0` (+1 more) | 5000 | 0..=32767 | competition 0x2f, rounds below 0x1b: a club in division 15 gets this as its both-clubs amount (home and away; vanilla 5000) |
| `demand_status_pct` | `0x8200b280`, 12 words | a table of 12 | -100..=200 | the % on his wage demand by buyer status minus his club's status, -6..5 |
| `development_by_age` | `0x8200bbe4`, 10 words | a table of 10 | 0..=100 | [growth, decline %] per age band (under 21, 21-25, 26-30, 31-33, 34+); vanilla 35,20 30,20 25,20 10,30 5,50 |
| `development_band1_age` | `0x82196c98` | 21 | 0..=60 | the youngest age in development band 1 (vanilla 21) |
| `development_band2_age` | `0x82196cb4` | 26 | 0..=60 | the youngest age in development band 2 (vanilla 26) |
| `development_band3_age` | `0x82196ccc` | 31 | 0..=60 | the youngest age in development band 3 (vanilla 31) |
| `development_band4_age` | `0x82196ce0` | 34 | 0..=60 | the youngest age in development band 4 (vanilla 34) |
| `young_match_growth` | `0x8200bc10`, 18 words | a table of 18 | 0..=100 | rows [from, to, bonus] by match rating 0..100 for players under 24; vanilla bonus 0,10,20,30,40,50 for 0-50..90-100 |
| `young_match_growth_age` | `0x82196a58` | 24 | 0..=60 | players younger than this develop from their matches (vanilla 24) |
| `sub_label_rows` | `0x8220c928` | 16 | 16..=20 | team screen rows below this (from row 12) are labelled as substitutes (vanilla 16; bench = 9 sets 20) |

**Calendar** (#703): with `base_year = 2026` alone, the disc's 2006 fixtures keep their calendar
dates, so a Saturday round lands on a Wednesday. `calendar_weekdays = true` moves every round to
its old weekday (+3 days in 2026/27, the Premier League opening Sat 15 Aug), and `calendar_weeks = 1`
a week later on top: the real 2026/27 opening days (Premier League Sat 22 Aug, EFL Sat 15 Aug).
Each season moves as one block, so no round passes another; a season whose late-June rounds can't
take the extra week keeps its weekdays only. `calendar_weeks` takes 0 or 1 (more pushes late
rounds past the season's end) and needs `calendar_weekdays`. Only a new career reads the calendar.

**Champions League league phase** (`league_phase = true`, #703, experimental): the European Cup in
the 2024+ shape. 36 clubs (the 16 straight entrants, the 16 qualifiers and the best-placed UEFA Cup
club of each of the first 4 countries) play one league of 8 matchdays, 2 opponents from each pot of
9, one at home and one away, never a club of their own country. 1st-8th go to the last 16, 9th-24th
to a two-legged play-off in February, 25th-32nd to the UEFA Cup's last 32. Only a new career reads
it. The table screens still show 4-club groups (to come).

**Formations** (`formations = "formations.csv"`, #691, #749): `id,name,roles[,spots]`, ids 0-13
change a shape, 14 and 15 add one (the game's 4-bit field holds 16). Roles are the 11 slots as
`GK FB SW CB WB CM WM AM CF WF` (slot 1 the goalkeeper); `spots` (the pitch diagram's grid, 0-33)
can be left out for a name like `4-2-3-1`. A career saved with its team on shape 14 or 15 needs
the mod that added them: loaded without it, the match engine reads that shape past the game's
14-row tables.
