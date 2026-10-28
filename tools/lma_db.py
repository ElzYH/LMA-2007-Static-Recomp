"""LMA Manager 2007 (Xbox 360) database library.

Reads and writes Common/GameData/Misc/Export.mng (and ExportPR.mng), the "BG2" archive
that holds the club / player database.

Formats (all big-endian, reverse-engineered from the retail disc, see FORMATS.md):

  BG2 archive   "BG2\\0", u32 0x14, u32 table_bytes, u32 data_start(0x1800), u32 data_len,
                then N entries of {char name[0x100]; u32 offset; u32 size} (offset relative to
                data_start), files aligned to 0x800.
  Table (*.bin) u32 magic 0x4C3EB25B, u32 record_size, u32 count, then records.
  Name pool     u32 offsets[n] (absolute), then cp1252 NUL-terminated strings.
  Players.bin   40-byte records read as one 320-bit big-endian integer, fields MSB-first.
  Clubs.bin     84-byte records; squad = 32 x u16 player ids at +18 (0xFFFF = empty).
"""
import struct

DB_MAGICS = (0x4C3EB25B, 0x5ABEB540)  # Export.mng, ExportPR.mng
ALIGN = 0x800
ENC = "cp1252"

# (column, bit position, width, add) - stored value = column value - add
PLAYER_FIELDS = [
    ("surname_id", 3, 15, 0),
    ("forename_id", 18, 14, 0),
    ("tackling", 32, 7, 0),
    ("marking", 39, 7, 0),
    ("passing", 46, 7, 0),
    ("technique", 53, 7, 0),
    ("position", 60, 2, 0),     # 0 GK, 1 DEF, 2 MID, 3 ATT
    ("position2", 62, 2, 0),    # secondary position, 0 = none
    ("shot_accuracy", 64, 7, 0),
    ("heading", 71, 7, 0),
    ("temperament", 78, 7, 0),
    ("pace", 85, 7, 0),
    ("fitness", 96, 7, 0),      # base fitness; the squad screen shows current (pre-season lower)
    ("intelligence", 103, 7, 0),
    ("nation_id", 110, 8, 0),
    ("height_cm", 129, 6, 150),
    ("dribbling", 135, 7, 0),
    ("creativity", 142, 7, 0),
    ("runs", 149, 7, 0),
    ("positioning", 176, 7, 0),
    ("age", 183, 5, 16),
    ("squad_number", 192, 7, 0),
    ("stamina", 205, 7, 0),
    ("strength", 224, 7, 0),
    ("shot_power", 231, 7, 0),  # 1 for goalkeepers
]
PLAYER_RS = 40
CLUB_RS = 84
CLUB_SQUAD_OFF = 18
CLUB_SQUAD_MAX = 32
CLUBNAME_LEN = 26
SHORTNAME_LEN = 8
STADNAME_LEN = 40
STADIUM_RS = 32
STANDS_OFF = 16  # 4 x u16 stand capacities: north, east, south, west
MANAGER_RS = 36   # Managers.bin: record i = manager of club i, then unattached managers
MGR_SURNAME = (11, 15)  # bit position, width (index into Surnames.bin)
LANGS = "EFGIS"


# ---------------------------------------------------------------- BG2 archive
class Archive:
    def __init__(self, path):
        d = open(path, "rb").read()
        magic, hsz, tbytes, dstart, dlen = struct.unpack_from(">4sIIII", d, 0)
        assert magic == b"BG2\0" and hsz == 0x14, "not a BG2 archive"
        self.dstart = dstart
        self.names, self.files = [], {}
        for i in range(tbytes // 0x108):
            e = 0x14 + i * 0x108
            name = d[e:e + 0x100].split(b"\0")[0].decode("ascii")
            off, size = struct.unpack_from(">II", d, e + 0x100)
            self.names.append(name)
            self.files[name] = d[dstart + off:dstart + off + size]
        self.orig = d

    def build(self):
        tbytes = len(self.names) * 0x108
        body, table = bytearray(), bytearray()
        for name in self.names:
            data = self.files[name]
            off = len(body)
            body += data
            body += b"\0" * (-len(body) % ALIGN)
            table += name.encode("ascii").ljust(0x100, b"\0") + struct.pack(">II", off, len(data))
        # the original drops the padding after the last file
        body = body[:off + len(self.files[self.names[-1]])]
        head = struct.pack(">4sIIII", b"BG2\0", 0x14, tbytes, self.dstart, len(body)) + table
        head += b"\0" * (self.dstart - len(head))
        return bytes(head + body)


# ---------------------------------------------------------------- helpers
def read_table(blob):
    magic, rs, n = struct.unpack_from(">III", blob, 0)
    assert magic in DB_MAGICS, f"bad table magic {magic:#x}"
    return magic, rs, [blob[12 + i * rs:12 + (i + 1) * rs] for i in range(n)]


def write_table(magic, rs, recs):
    return struct.pack(">III", magic, rs, len(recs)) + b"".join(recs)


class NamePool:
    """Forename.bin / Surnames.bin. Indices of existing names never move."""

    def __init__(self, blob):
        n = struct.unpack_from(">I", blob, 0)[0] // 4
        offs = struct.unpack_from(f">{n}I", blob, 0)
        self.names = [blob[o:blob.index(b"\0", o)].decode(ENC) for o in offs]
        self.orig = blob
        self.lookup = {}
        for i, s in enumerate(self.names):
            self.lookup.setdefault(s, i)
        self.dirty = False

    def index(self, s):
        if s not in self.lookup:
            s.encode(ENC)  # raises if the game font can't hold it
            self.lookup[s] = len(self.names)
            self.names.append(s)
            self.dirty = True
        return self.lookup[s]

    def build(self):
        if not self.dirty:
            return self.orig
        n = len(self.names)
        offs, pool, pos = [], bytearray(), 4 * n
        for s in self.names:
            offs.append(pos + len(pool))
            pool += s.encode(ENC) + b"\0"
        return struct.pack(f">{n}I", *offs) + bytes(pool)


def get_bits(rec, pos, w):
    v = int.from_bytes(rec, "big")
    return (v >> (len(rec) * 8 - pos - w)) & ((1 << w) - 1)


def set_bits(rec, pos, w, val):
    if not 0 <= val < (1 << w):
        raise ValueError(f"value {val} does not fit in {w} bits")
    nbits = len(rec) * 8
    v = int.from_bytes(rec, "big")
    sh = nbits - pos - w
    v = (v & ~(((1 << w) - 1) << sh)) | (val << sh)
    return v.to_bytes(len(rec), "big")


def fixed_strings(blob, size):
    return [blob[i:i + size].split(b"\0")[0].decode(ENC) for i in range(0, len(blob), size)]


def pack_fixed(names, size):
    out = bytearray()
    for s in names:
        b = s.encode(ENC)
        if len(b) > size - 1:
            raise ValueError(f"'{s}' longer than {size - 1} bytes")
        out += b.ljust(size, b"\0")
    return bytes(out)


# ---------------------------------------------------------------- database
class Database:
    def __init__(self, path):
        self.ar = Archive(path)
        f = self.ar.files
        self.forenames = NamePool(f["Forename.bin"])
        self.surnames = NamePool(f["Surnames.bin"])
        self.magic, _, self.players = read_table(f["Players.bin"])
        _, _, self.clubs = read_table(f["Clubs.bin"])
        self.club_names = {l: fixed_strings(f[f"ClubNam{l}.bin"], CLUBNAME_LEN) for l in LANGS}
        self.short_names = {l: fixed_strings(f[f"ShortNm{l}.bin"], SHORTNAME_LEN) for l in LANGS}
        self.stadium_names = fixed_strings(f["StadName.bin"], STADNAME_LEN)
        mg = f["Managers.bin"]
        self.managers = [mg[i:i + MANAGER_RS] for i in range(0, len(mg), MANAGER_RS)]
        st = f["Stadiums.bin"]
        self.stadiums = [st[i:i + STADIUM_RS] for i in range(0, len(st), STADIUM_RS)]

    # managers -----------------------------------------------------------
    def manager_surname(self, i):
        pos, w = MGR_SURNAME
        return self.surnames.names[get_bits(self.managers[i], pos, w)]

    def set_manager_surname(self, i, name):
        pos, w = MGR_SURNAME
        self.managers[i] = set_bits(self.managers[i], pos, w, self.surnames.index(name))

    # stadiums -----------------------------------------------------------
    def stands(self, ci):
        return list(struct.unpack_from(">4H", self.stadiums[ci], STANDS_OFF))

    def set_stands(self, ci, caps):
        r = bytearray(self.stadiums[ci])
        struct.pack_into(">4H", r, STANDS_OFF, *caps)
        self.stadiums[ci] = bytes(r)

    # squads -------------------------------------------------------------
    def squad(self, ci):
        ids = struct.unpack_from(f">{CLUB_SQUAD_MAX}H", self.clubs[ci], CLUB_SQUAD_OFF)
        return [i for i in ids if i != 0xFFFF]

    def set_squad(self, ci, ids):
        if len(ids) > CLUB_SQUAD_MAX:
            raise ValueError(f"club {ci} squad has {len(ids)} players (max {CLUB_SQUAD_MAX})")
        slots = list(ids) + [0xFFFF] * (CLUB_SQUAD_MAX - len(ids))
        r = bytearray(self.clubs[ci])
        struct.pack_into(f">{CLUB_SQUAD_MAX}H", r, CLUB_SQUAD_OFF, *slots)
        self.clubs[ci] = bytes(r)

    def club_wealth(self, ci):
        """Top 4 bits of the club record: starting-funds tier (15 richest)."""
        return self.clubs[ci][0] >> 4

    def set_club_wealth(self, ci, tier):
        if not 0 <= tier <= 15:
            raise ValueError(f"wealth {tier} must be 0-15")
        r = bytearray(self.clubs[ci])
        r[0] = (tier << 4) | (r[0] & 0x0F)
        self.clubs[ci] = bytes(r)

    def is_placeholder(self, ci):
        """Historic/special clubs (e.g. Wanderers, World Cup Team) have an all-zero squad list."""
        return self.clubs[ci][CLUB_SQUAD_OFF:CLUB_SQUAD_OFF + 2 * CLUB_SQUAD_MAX] == bytes(2 * CLUB_SQUAD_MAX)

    def owners(self):
        own = {}
        for ci in range(len(self.clubs)):
            if self.is_placeholder(ci):
                continue
            for slot, pid in enumerate(self.squad(ci)):
                own.setdefault(pid, (ci, slot))
        return own

    # players ------------------------------------------------------------
    def player(self, pid):
        rec = self.players[pid]
        p = {c: get_bits(rec, pos, w) + add for c, pos, w, add in PLAYER_FIELDS}
        p["forename"] = self.forenames.names[p["forename_id"]]
        p["surname"] = self.surnames.names[p["surname_id"]]
        return p

    def set_player(self, pid, values):
        rec = self.players[pid]
        for c, pos, w, add in PLAYER_FIELDS:
            if c in values and values[c] != "":
                rec = set_bits(rec, pos, w, int(values[c]) - add)
        self.players[pid] = rec

    # write --------------------------------------------------------------
    def build(self):
        f = self.ar.files
        f["Forename.bin"] = self.forenames.build()
        f["Surnames.bin"] = self.surnames.build()
        f["Players.bin"] = write_table(self.magic, PLAYER_RS, self.players)
        f["Clubs.bin"] = write_table(self.magic, CLUB_RS, self.clubs)
        for l in LANGS:
            f[f"ClubNam{l}.bin"] = pack_fixed(self.club_names[l], CLUBNAME_LEN)
            f[f"ShortNm{l}.bin"] = pack_fixed(self.short_names[l], SHORTNAME_LEN)
        f["StadName.bin"] = pack_fixed(self.stadium_names, STADNAME_LEN)
        f["Stadiums.bin"] = b"".join(self.stadiums)
        f["Managers.bin"] = b"".join(self.managers)
        return self.ar.build()

    def save(self, path):
        open(path, "wb").write(self.build())
