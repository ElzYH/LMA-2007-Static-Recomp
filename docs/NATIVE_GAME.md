# The native game

Where lma-rs ends up, and how far along it is. the maintainer's decisions of 28 Oct (#61 comments 6466002525
and 6466030105). Update the tables in the PR that moves a number.

## End state

- **The whole game is hand-readable source**: Rust everywhere; Swift only for the Apple shell where
  Rust doesn't run well there (the maintainer, 28 Oct). No interpreter,
  no recompiled machine code, nothing run from the disc.
- **The recompiled code is temporary scaffolding.** It goes system by system: a system's readable
  rewrite passes the fingerprints (`tools/regress.py`: day, match and match-day fingerprints, frames),
  then that system's recompiled functions are deleted from the build.
- **Assets:** the first launch converts the player's ISO once into modern formats in `data/`: PNG,
  glTF, OGG/WAV, JSON/CSV. From then on the game loads only `data/`, never the ISO, and any asset
  can be replaced by dropping in a file.

## Minimum scope

These three groups become readable Rust first. Everything else may stay recompiled ("recompiled,
OK") until a mod or a bug needs it.

1. **Every moddable system:** contracts and wages, transfers, finances and the AI economy,
   competitions and the calendar, squads and AI squad management, formations and tactics, training,
   player ratings and development.
2. **Every performance hot path:** the renderer, audio, the match simulation loop, day processing.
3. **The platform layer:** input, saves, settings, the launcher, asset loading from `data/`.

## Where we are (28 Oct)

The recompiler's output today: **22,894 functions** in 32 crates, 1,622,138 instructions native
(recompiled), 10,099 through the interpreter (0.6%). A few dozen game functions are replaced or
wrapped by a native hook (`guest::hooks::ADDRESSES`, `runtime::hooks`), so 41 of the 18,645 functions in
the Ghidra export (**0.22%**) have a native hook so far. The per-system % below comes from `tools/native_progress.py` (#874): the functions reached
from the system's entry points (shared helpers with 25+ callers not walked through) that have a
native hook. Calls through function pointers and vtables count as calls (the match engine makes
most of its calls that way). A hook may wrap rather than replace, so it's an upper bound on what is rewritten, and
"reached" grows as more of a system's entry points are listed. Entry points are the listed addresses plus every function that references the system's own
source files (the game's assertion strings carry its .cpp paths).

### 1. Moddable systems

| system | readable now | % rewritten | scaffolding left | mod surface today |
|---|---|---|---|---|
| contracts and wages | the wage getter/rounding (`WAGE` `0x82123dc8`, `WAGE_ROUND` `0x82118e88`), the AI's wage offer (`AI_OFFER`), contract set; the terms log (#853, draft) | 5.8% hooked (5 of 86) | personal terms `0x82187de0` and the stature check `0x821884c0`, the contract screen | `wage_cap`, `ai_wage_offers`, `ai_signing_wage_cap`, `rating_wage_scale`; `[tune]` (#869): `prime_wage_premium`, `demand_tier_pct`, `demand_status_pct` |
| transfers | the search pre-filter (`SEARCH_FILTER` `0x821794c8`), the valuation (`VALUATION`, `VALUE_SET`, premiums) | 4.4% hooked (8 of 180) | bids `0x82189088`, asking price `0x82188f38`, the sale check `0x821889c0`, deal worth `0x8217c410` | `value_unit`, `values_as_stored`; `[tune]` (#869): `bid_pct_*`, `bid_raise_pct_*`, `asking_price_top_factor`, `deal_instalments_factor`, `exchange_value_*`, `sale_min_squad`, `sale_min_available` |
| finances and the AI economy | income (`ADD_FUNDS`, statements, gate receipts), sponsorship getters/setters and totals, wage budgets, `FUNDS` | 2.5% hooked (11 of 432) | the board's verdict `0x822bd6b8`, loans, the prize payments (league `0x820ec3d8`, cup `0x820ecae0`) | `income_scale`, `gate_scale`, `sponsor_*`, `board_share`, `wage_budget_cap`; `[tune]` (#869): `funds_tier`, `funds_nation_factor`, `funds_chelsea_factor`, `funds_cup_club_factor`, `board_verdict_levels_short`, `league_prize_territory*`, `cup_prize_*` |
| competitions and the calendar | European places (`EUROPE_PLACES`), the diary retime in `lma-data::diary` (data, not code) | 1.1% hooked (1 of 90) | fixtures generation, the group/knockout code (#703: the league-phase table is capped at 32 clubs by a stack buffer) | `europe_places`, `base_year`, `calendar_weekdays`, `calendar_weeks` |
| squads and AI squad management | player club lookup (`PLAYER_CLUB`), the bench slots (`SLOT_LOOKUP`, `SLOT_GET`, `MATCH_BUILD`, the team screen rows) | 6% hooked (7 of 117) | the AI's buying/selling targets (`0x8218c430`, `0x8218cd50`), squad count | `bench`, `subs`, `squad_overflow.csv`; `[tune]` (#869): `squad_max`, `squad_fill_base` |
| formations and tactics | the formation offsets and the pitch diagram slot (`FORMATION_*`) | 23% hooked (5 of 22) | the tactics record, the match engine's use of it | `formations.csv` |
| training | none | 0% (0 of 3) | all | none |
| player ratings and development | overall and role (`PLAYER_OVERALL`, `PLAYER_ROLE`) | 9% hooked (2 of 23) | ageing, form, growth, regens (`0x82120d38`) | `players.csv` (database) |

### 2. Performance hot paths

| system | readable now | % rewritten | scaffolding left |
|---|---|---|---|
| renderer | the wgpu backend (Metal/Vulkan/DX12), the PM4 command processor, EDRAM surfaces and resolves; HLE D3D (#171) hooks the D3D entry points: observe-only on main (#729), native DrawIndexedVertices/SetTexture/filters (#731) and the effect-state commit's float loops (#843) in drafts; single-pass rendering (`LMA_SINGLE_PASS`, the band replay collapsed to one pass, #876) in test | 6.7% hooked (6 of 89) | the game's D3D runtime (state setters, the commit, the tiled replay) and the Xenos command stream it writes; the CP/EDRAM path is the fallback until the HLE path draws every frame |

### 3. Platform layer

| system | readable now | scaffolding left |
|---|---|---|
| input | lma-app (keyboard, pads, glyphs per device, co-op) into the kernel's XInput HLE | the game's own input handling |
| saves | the kernel's file HLE writes the game's own save files to `userdata/`; save states (#788) are the emulator's | the game's save format and code |
| settings | lma-app's settings and F2 menu | the game's options screens |
| launcher | lma-app (native, the game's fonts and sounds) | none |
| asset loading from `data/` | none yet: the game reads the disc's archives (`.mng`) through the VFS | everything (see the converter below) |

### Recompiled, OK

Everything not listed above: menus and UI screens, the news and messages, the game's string and
archive handling, the kernel thread and sync code under the HLE, and so on. These stay recompiled
until a mod or a bug needs one of them.

## Milestone: the asset converter

The first launch reads the player's ISO once and writes `data/`. The steps, each its own PR with a
round-trip test (convert, load the converted file in the game, frames and fingerprints unchanged):

| asset | from (on the disc) | to | status |
|---|---|---|---|
| database (players, clubs, competitions, history) | `Export.mng` | CSV | `lma-cli db-export` / `db-import` exist; the game still loads the `.mng` |
| season diaries | `Diary20.mng` | JSON | `lma-cli diary <Export.mng> [season]` exports every round and transfer window as CSV; a JSON form for the converter not written |
| strings | the string tables (`LMA_<E,F,G,I,S>.lng`) | CSV | `lma-cli strings <Lang dir> out.csv`: every string of every table, `id,text,lang` (the mods' `strings.csv` shape); 6,812 strings x 5 languages |
| audio | XMA in XACT wave banks | WAV (OGG for size) | `lma-cli sounds <bank.xwb or Sound.mng> <cache_dir> <out_dir> [bank.xsb]`: one WAV per wave from the PCM cache the install writes (#772), plus `cues.json` (cue name, sound, wave, looping, volume, pitch) from the sound bank; the game still plays from the cache, not from `data/` files; OGG for the long beds not done |
| fonts | TIM2 sheet + PCF metrics in `Fonts.mng` | PNG + JSON | the sheets export with `lma-cli textures` (#879); the PCF metrics are read by `lma-data::font` (#803, open), not exported yet |
| textures | DDS (DXT1/3/5, 32-bit) and TIM2 (4/8-bit palettised, direct colour) inside the archives | PNG | `lma-cli textures <file.mng> <out_dir>` (#879, `lma_data::textures`): every texture archive on the disc decodes, 0 failures (51,084 kit PNGs in 15 s); level 0 only, mips not kept |
| models | `.p3d` / `.p3s` (Codemasters' own: `D3PC` tag, a version word 02 09 01 00, a size, then big-endian floats that look like a bounding box; chunked) | glTF | not started: the format is unmapped (10,403 files, 1.4 GB); the first job is the chunk layout, then vertices/indices, then skeletons (`.cxk`/`.mok`/`.kfc`) |
| badges | `.tm2` in `Badge.mng` and in mods | PNG | both ways now: `lma-cli textures` exports them (323 on the disc, #879), `tools/badge_pack.py` builds them from PNGs |

**`.p3d` survey (mac-ci, 28 Oct; 25 files from TrainingGrounds.mng and Stadiums.mng, every one the same layout):**
a relocatable memory image with its pointer list, not a tagged chunk format. Big-endian throughout; every pointer is
a file offset (an in-image offset, never a tag).

- Header (0x10): `D3PC`, version `02 09 01 00`, u32 `image_end` (the end of the in-memory part, which #883 read as a
  size), u32 0. S_Int models carry a bounding box (two float triples) from 0x10; the training-ground and stadium files
  have zeros there. At 0xe0 a 4x4 identity (the root transform), at 0x120 the pointer to the first node.
- Nodes: a linked chain through the fix-ups (`0x120 -> 0x1f0 -> 0x2b0 -> 0x370 ...`, 0xc0 bytes a node after the first).
- Just before `image_end`: the name pool, 9-byte NUL-terminated names (`t03_1_c1`, `t03_1_d1`: material/mesh names).
- Trailer at `image_end` (14 words): [1] = 0xa0, [4] = name pool end, [5]/[6] = name table offset and count
  (17 pointers, one per name), [8]/[9] = reference table count and offset (always `image_end + 0x38`), [10]/[11] =
  fix-up count and offset (ends exactly at the file's end), [13] = 4. The reference table is (pointer, kind) pairs,
  kind 0 or 3 only (608 and 320 over the 25 files): kind 3 points at a struct that starts `0 0 1 ptr ptr ptr`, kind 0
  at `0 1 0 ptr 0x2a1 0`. The fix-up list is every pointer slot in the file, in object order (not sorted), the
  trailer's own pointers included; its first word is 4 (slot 0x4 holds the version, so a flag, not a slot).
- Strings in the image: shader parameter names (`legacytag.fx`, `gf4x4WorldViewProj`, `gTexture0`, ...) and texture
  names (`t04_1_t2`), so materials carry their effect and sampler bindings by name.
- The game's loader (28 Oct, `LMA_FILE_BUF=1`, #889): the match load reads a model whole into a heap buffer through
the read helper `0x823b3938` from `0x822e5028`, under the asset-load routine `0x822e5380` (508 bytes, 31 callers;
it reads the image's `+8` word, `image_end`), called from the match set-up (`0x82222498`, `0x82222550`,
`0x82222988`). A write watch on the image's root-pointer slot (`+0x120`) saw no in-place relocation before the
buffer was reused 82 M instructions later: the image is consumed out of a staging buffer (copied or resolved
into the game's own structures), so the fix-up list is read, not applied in place. `0x822e5380`'s helpers
`0x822e4580`, `0x822e4648`, `0x822e47b8`, `0x822e48a0`, `0x822e4f28` and `0x822e50c0` are where the trailer
walk lives; which one consumes the fix-up list is the next question.
Second round (mac-ci's call graphs + two read watches, `rwatch` feature): the six helpers are allocators and the
file side; the image goes from `0x822e4f28` / `0x822e50c0` to `0x822e73f8` and `0x822e7350`, and `0x822e7350` ->
`0x822ed278` / `0x822ea318` (the per-node visitor) -> `0x823afe18` -> the D3D texture and buffer entries
(`0x82338d08`, `0x8233a750`, `0x823b0480`). Neither the trailer's fix-up-offset word nor the root-pointer slot is
read by guest code in the 80 M instructions before the staging buffer is reused, so the fix-up list is never
walked and the image is consumed by a path the read watch can't see (a copy through the HLE or vector loads,
or the bytes handed to the renderer as buffers). The bake lives under `0x822e7350`; its node walk is the next
thing to read in Ghidra.
Why (mac3, 28 Oct): on this title the D3D runtime takes texture headers and vertex/index-buffer descriptors as
guest pointers into memory the game owns, so the loader hands the image's bytes to the renderer in place
(`0x82338d08` SetTexture gets a header address, the buffer entries base + size) and never relocates; the fix-up
list is a PC-tool artefact the console build ignores. For the converter that means: a `.p3d` is a set of
textures, vertex and index buffers plus a node tree over them, all addressed by offset from the image start.
Not mapped yet: the node and mesh structs themselves (vertex/index buffers, formats), which the fix-up chain and the
  reference table now make reachable. Survey script: a 60-line Python BG2 walker + probe, in the mac-ci scratchpad
  (nothing committed; the files are the player's).

### What is on the disc (converter input, surveyed 28 Oct)

Counted on the Mac's disc copy by magic and name (counts and formats only; the files themselves never
leave the player's machine). 232 files, 4.8 GB: `default.xex`, `Common/` (3.5 GB) and five
`Data_0N/` folders (commentary wave banks per language). Nearly everything sits inside `BG2`
archives (`*.mng`, `lma-data::archive`), nested up to three deep.

| what | where | leaf files | format | converter target |
|---|---|---|---|---|
| database | `GameData/Misc/Export.mng` (+ `ExportPR.mng`) | 86 `.bin` tables (62 + 24) | the `lma-data` database (`db.rs`, `diary.rs`, `fixtures.rs`) | JSON/CSV (`db-export` exists) |
| strings | `GameData/Misc/Lang/LMA_*.lng` | 5 languages | `lma-data::lang` | CSV per language |
| kit textures | `GameData/Textures/Strips.mng` (1.06 GB): one `.mng` per club, one per kit inside | 51,084 `.dds` | DXT textures, Xbox 360 byte order | PNG |
| stadium models | `GameData/Models/Stadiums.mng` (457 MB), `StadKpts.mng` (211 MB), `TrainingGrounds.mng` | 7,140 + 1,908 + 5 `.p3d`; 27,837 + 12,157 + 113 `.dds`; 2,023 `.dat` (all empty: placeholders) | `.p3d` mesh (Codemasters' own, not mapped), `.dat` per-stadium tables | glTF + PNG + JSON |
| match-day models and animation | `MatchHi/Models/*.mng` (S_Int_*: 1.3 GB) | 1,321 `.p3d`, 92 `.p3s`, 537 `.cxk` + 537 `.mok` (animation), 61 `.kfc`, 104 `.cut`, 308 `.dmy`, 105 `.dlt_xb2`, 2,656 `.dds` | mesh, skeleton, keyframes (not mapped) | glTF (+ animations) + PNG |
| match-day textures | `MatchHi/Textures/*.mng` (faces, hair, boots) | in the `.dds` count above | DXT | PNG |
| 2D textures and fonts | `GameData/Textures/Textures_2D.mng`, `Textures_3D.mng`, `Fonts.mng`, `Font.dds` + `Font.pcf` | 1,063 `.tm2`, 927 `.dds`, 8 `.pcf` + 8 `.tm2` | TIM2 sheets, DXT; PCF glyph metrics (`lma-data::font`, #803) | PNG + JSON |
| front-end art | `Frontend/Textures`, `Frontend/Misc` | 323 `.tm2`, 21 `.cxk` + 21 `.mok` | TIM2; menu animation | PNG, JSON |
| audio | `Data_0N/*.xwb` + `.xsb` (commentary, five languages), `Frontend/Sound`, `MainGame/Sound`, `MatchHi/Sound/*.mng` (crowd, dugouts: 15 banks inside) | 42 + 5 + 15 wave banks, the same sound banks, 1 `.xgs` | XACT: XMA waves (`lma-data::xact`, decoded once by #772) | OGG (long beds) / WAV (cues) + JSON cue tables |
| video | `Frontend/Misc/*.xmv` | 3 (intro, attract) | XMV | MP4/WebM, or dropped (never played today) |
| shaders | `GameData/Effects/*.fx` + `.updb` | 23 + 18 | Xenos shader binaries and debug db | not converted: the native renderer has its own (`shader_wgsl`) |
| misc tables | `GameData/Misc/*.bin`, `.dat`, `.pub`, `.ini`, `MatchHi/Misc/*.fee` | 6 | small custom tables (sponsors, fees) | JSON once each is read |

Totals inside every archive, from `lma-cli archive <file.mng> ls` over the 78 `.mng` files (#875):
99,063 `.dds` (1.75 GB), 10,403 `.p3d` (1.40 GB), 1,394 `.tm2`, 537 `.cxk` + 537 `.mok` + 61 `.kfc` +
104 `.cut` (animation), 308 `.dmy`, 92 `.p3s`, 93 `.bin`, 31 `.xwb` + 31 `.xsb` (252 MB), 8 `.pcf`,
105 `.dlt_xb2` and a handful of other `_xb2` files, 2,023 empty `.dat`. Loose on the disc besides
the archives: 42 `.xwb` + 42 `.xsb` commentary banks, 23 `.fx`, 18 `.updb`, 5 `.lng`, 3 `.xmv`.

Not mapped yet, and the converter's real work: the `.p3d`/`.p3s` mesh format and the `.cxk`/`.mok`/`.kfc`
animation format (Codemasters' own), and the `.tm2`/`.dds` texture variants as the 360 stores them
(the runtime already decodes them from guest memory, `raster.rs`; the converter reuses that decoder).

Done so far (28 Oct), all in `lma-cli`: `archive ls|extract` (#875), `textures` (#879), `strings` (#885),
`sounds` (#886), each over the nested BG2 archives; `db-export` and `diary` predate the milestone. Left, in
order: the `.p3d` meshes and their animation (the survey above; the loader is found by a headless run with the
file-open log), the PCF font metrics (#803), the diary as JSON, OGG for the long audio beds, and then the
loader side: the game reading `data/` instead of the disc.

The game then reads `data/` through one asset loader (platform layer above), and the VFS's disc
path is deleted.

## How a system moves over

1. Find the system's entry points and the functions reachable from them (Ghidra; `tools/native_progress.py`,
   planned, will count them).
2. Write it as readable Rust with its numbers named (the tunables table, #870, is where these
   names start: a rewritten system reads the same `[tune]` names, so mods keep working).
3. Install it as a hook over the recompiled entry point; fingerprints and frames must match
   vanilla (`tools/regress.py`), plus a driven check of the system's behaviour where the
   fingerprints don't reach (a transfer window, a season).
4. Delete the recompiled functions only it reached; update the table above.
