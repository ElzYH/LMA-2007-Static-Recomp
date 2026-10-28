#!/usr/bin/env python3
"""Adds FM's per-player skin tone, hair colour, hair type and hair length to an fmsave players
export, for squad-import's `[fm] skin` / `hair_colour` / `hair_type` / `hair_length` (#31 item 5,
#697: LMA's skin and hair fields came from the 2006 record each import overwrote, so Saka and
Mbappé came out light and Gyökeres dark-haired).

  tools/fm_skin.py <save.fm> <players.csv from fmsave> <out.csv>

fmsave 0.5 decodes no appearance fields, but FM keeps them in each player's person block at fixed
offsets from the birth date fmsave anchors that block on: the skin tone at +8 (1 lightest .. 20
darkest, 0 unknown; found by comparing West African and Nordic/Eastern European nations: effect
size 5.5, where no other byte reaches 1.5), and (#697, 21 Oct, 7,456 Norwegians/Swedes against
11,057 dark-skinned players of the maintainer's save) the hair colour at +6 (low = light: Haaland 7,
Gyökeres 8, Ødegaard 10, Kane 9, Saka 12, Rice 12, Mbappé 15, Salah 15), a hair type at +5 (0 for
most, 2-3 for afro/curly: Saka, Mbappé, Salah) and the hair length at +7 (1 short, Haaland 3).
This reads the save once through fmsave, notes each player's anchor, and writes the export with
`skin_tone`, `hair_colour`, `hair_type` and `hair_length` columns. The save is only read. The
output is derived from the save: keep it beside the export (both git-ignored), never in the repo.
"""
import bisect, csv, sys

import fmsave
from fmsave.readers import persons
from fmsave.readers._common import GAME_DB_SECTION

# the appearance bytes, from the person block's birth-date anchor
SKIN_FROM_BIRTH = 8
HAIR_COLOUR_FROM_BIRTH = 6
HAIR_TYPE_FROM_BIRTH = 5
HAIR_LENGTH_FROM_BIRTH = 7


def main(save_path, export_path, out_path):
    anchors = []
    try_candidate = persons.PersonBlockDecoder._try_candidate

    def noting(self, game_db, birth_date_offset, window_end):
        person = try_candidate(self, game_db, birth_date_offset, window_end)
        if person is not None:
            anchors.append(birth_date_offset)
        return person

    persons.PersonBlockDecoder._try_candidate = noting
    save = fmsave.open(save_path)
    save.players()
    records = save._context.player_records()
    offsets = list(records.record_offsets)
    look = {}
    with save._context.section(GAME_DB_SECTION) as db:
        for a in sorted(anchors):
            position = bisect.bisect_right(offsets, a) - 1
            look.setdefault(records.uids[position], (db[a + SKIN_FROM_BIRTH], db[a + HAIR_COLOUR_FROM_BIRTH], db[a + HAIR_TYPE_FROM_BIRTH], db[a + HAIR_LENGTH_FROM_BIRTH]))
    with open(export_path, encoding='utf-8-sig', newline='') as f:
        reader = csv.reader(f)
        header = next(reader)
        uid = header.index('uid')
        rows = list(reader)
    with open(out_path, 'w', encoding='utf-8', newline='') as f:
        w = csv.writer(f)
        w.writerow(header + ['skin_tone', 'hair_colour', 'hair_type', 'hair_length'])
        for r in rows:
            tone, colour, kind, length = look.get(int(r[uid]), (None, None, None, None)) if r[uid].isdigit() else (None, None, None, None)
            w.writerow(r + ['' if tone is None or tone > 20 else tone, '' if colour is None or colour > 40 else colour, '' if kind is None or kind > 20 else kind, '' if length is None or length > 20 else length])
    print(f'{len(rows)} players, {sum(1 for t in look.values() if 0 < t[0] <= 20)} with a skin tone, {sum(1 for t in look.values() if 0 < t[1] <= 40)} with a hair colour -> {out_path}')


if __name__ == '__main__':
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    main(*sys.argv[1:])
