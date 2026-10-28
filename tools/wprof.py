#!/usr/bin/env python3
"""Sampling profiler for one thread of a running Windows process (lma-app, lma-cli), with stack
walks through dbghelp. No installs, no admin: Python 3 and the system's dbghelp.dll.

    python tools/wprof.py <pid> <thread> [--samples N] [--hz H] [--top N]

<thread> is a thread name ('render', 'command processor', 'audio') or '@busiest' for the thread
with the most CPU time so far (the game thread, which has no name). Symbols come from the exe's
PDB (MSVC builds; for clean attribution build with debuginfo and `-C link-arg=/OPT:NOICF`, so
the linker doesn't fold identical functions under one name) and from system DLLs' exports.

Prints, for the samples taken:
- self time by leaf function;
- each leaf with its first caller in our code (where a memmove or heap allocation comes from);
- inclusive time of our functions (on the stack in that share of samples);
- with `--lines REGEX`: for samples inside a function whose name matches, the source line being
  run there (or calling out, for a leaf deeper down), so inlined code shows up line by line;
  `--leaf REGEX` keeps only samples whose leaf matches (`--leaf RtlAllocateHeap --lines draw`:
  which lines allocate).
The thread is suspended for each sample (a few microseconds): numbers are shares of samples, not
of wall time, and the profiled program runs slightly slower meanwhile."""
import argparse
import collections
import ctypes
import ctypes.wintypes as wt
import re
import sys
import time

ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument('pid', type=int)
ap.add_argument('thread')
ap.add_argument('--samples', type=int, default=2000)
ap.add_argument('--hz', type=float, default=400)
ap.add_argument('--max-suspended', type=float, default=0.1, help='the most of the time the thread may spend suspended for stack walks (the rate drops if walks are slow)')
ap.add_argument('--top', type=int, default=25)
ap.add_argument('--lines', help='regex: split the time in matching functions by source line')
ap.add_argument('--leaf', help='regex: only samples whose leaf matches (with --lines: where those calls come from, e.g. --leaf RtlAllocateHeap)')
a = ap.parse_args()

k32 = ctypes.WinDLL('kernel32', use_last_error=True)
for name, res in [('OpenProcess', wt.HANDLE), ('OpenThread', wt.HANDLE), ('CreateToolhelp32Snapshot', wt.HANDLE), ('SuspendThread', wt.DWORD)]:
    getattr(k32, name).restype = res
k32.SuspendThread.argtypes = [wt.HANDLE]
k32.ResumeThread.argtypes = [wt.HANDLE]
k32.GetThreadContext.argtypes = [wt.HANDLE, ctypes.c_void_p]
k32.GetThreadDescription.argtypes = [wt.HANDLE, ctypes.POINTER(ctypes.c_wchar_p)]
k32.GetThreadTimes.argtypes = [wt.HANDLE] + [ctypes.c_void_p] * 4


class THREADENTRY32(ctypes.Structure):
    _fields_ = [('dwSize', wt.DWORD), ('cntUsage', wt.DWORD), ('th32ThreadID', wt.DWORD), ('th32OwnerProcessID', wt.DWORD),
                ('tpBasePri', wt.LONG), ('tpDeltaPri', wt.LONG), ('dwFlags', wt.DWORD)]


def cpu_time(h):
    t = [wt.FILETIME() for _ in range(4)]
    if not k32.GetThreadTimes(h, *[ctypes.byref(x) for x in t]):
        return 0
    return sum(x.dwHighDateTime << 32 | x.dwLowDateTime for x in t[2:])


# the thread: by name, or the busiest
snap = k32.CreateToolhelp32Snapshot(0x4, 0)
te = THREADENTRY32()
te.dwSize = ctypes.sizeof(te)
target, best = None, (0, None)
ok = k32.Thread32First(snap, ctypes.byref(te))
while ok and not target:
    if te.th32OwnerProcessID == a.pid:
        h = k32.OpenThread(0x0002 | 0x0008 | 0x0040 | 0x0800, False, te.th32ThreadID)
        if h:
            if a.thread == '@busiest':
                c = cpu_time(h)
                if c > best[0]:
                    best = (c, h)
            else:
                p = ctypes.c_wchar_p()
                if k32.GetThreadDescription(h, ctypes.byref(p)) >= 0 and p.value == a.thread:
                    target = h
    ok = k32.Thread32Next(snap, ctypes.byref(te))
k32.CloseHandle(snap)
if a.thread == '@busiest':
    target = best[1]
if not target:
    sys.exit(f'no thread {a.thread!r} in process {a.pid}')
hp = k32.OpenProcess(0x0400 | 0x0010, False, a.pid)

dbg = ctypes.WinDLL('dbghelp')
dbg.SymSetOptions(0x2 | 0x4)  # undecorated names, deferred loads
dbg.SymInitializeW.argtypes = [wt.HANDLE, ctypes.c_wchar_p, wt.BOOL]
# search path: the exe's folder (its PDB sits next to it) and System32
k32.QueryFullProcessImageNameW.argtypes = [wt.HANDLE, wt.DWORD, ctypes.c_wchar_p, ctypes.POINTER(wt.DWORD)]
img = ctypes.create_unicode_buffer(1024)
n = wt.DWORD(1024)
k32.QueryFullProcessImageNameW(hp, 0, img, ctypes.byref(n))
exe_dir = img.value.replace(chr(92), '/').rsplit('/', 1)[0]
dbg.SymInitializeW(hp, exe_dir + ';C:/Windows/System32', True)
dbg.SymFunctionTableAccess64.restype = ctypes.c_void_p
dbg.SymFunctionTableAccess64.argtypes = [wt.HANDLE, ctypes.c_uint64]
dbg.SymGetModuleBase64.restype = ctypes.c_uint64
dbg.SymGetModuleBase64.argtypes = [wt.HANDLE, ctypes.c_uint64]
# dbghelp's own routines as the walk's callbacks, passed as C function pointers: Python callbacks
# cost two trips into the interpreter per frame, with the thread suspended the whole time (#145:
# deep recompiled stacks at 400 Hz froze the game)
FTA = ctypes.cast(dbg.SymFunctionTableAccess64, ctypes.c_void_p)
GMB = ctypes.cast(dbg.SymGetModuleBase64, ctypes.c_void_p)


class ADDRESS64(ctypes.Structure):
    _fields_ = [('Offset', ctypes.c_uint64), ('Segment', ctypes.c_ushort), ('Mode', ctypes.c_int)]


class STACKFRAME64(ctypes.Structure):
    _fields_ = [('AddrPC', ADDRESS64), ('AddrReturn', ADDRESS64), ('AddrFrame', ADDRESS64), ('AddrStack', ADDRESS64),
                ('AddrBStore', ADDRESS64), ('FuncTableEntry', ctypes.c_void_p), ('Params', ctypes.c_uint64 * 4),
                ('Far', wt.BOOL), ('Virtual', wt.BOOL), ('Reserved', ctypes.c_uint64 * 3), ('KdHelp', ctypes.c_uint64 * 20)]


class SYMBOL_INFOW(ctypes.Structure):
    _fields_ = [('SizeOfStruct', wt.ULONG), ('TypeIndex', wt.ULONG), ('Reserved', ctypes.c_uint64 * 2), ('Index', wt.ULONG),
                ('Size', wt.ULONG), ('ModBase', ctypes.c_uint64), ('Flags', wt.ULONG), ('Value', ctypes.c_uint64),
                ('Address', ctypes.c_uint64), ('Register', wt.ULONG), ('Scope', wt.ULONG), ('Tag', wt.ULONG),
                ('NameLen', wt.ULONG), ('MaxNameLen', wt.ULONG), ('Name', ctypes.c_wchar * 1024)]


dbg.StackWalk64.argtypes = [wt.DWORD, wt.HANDLE, wt.HANDLE, ctypes.POINTER(STACKFRAME64)] + [ctypes.c_void_p] * 5
dbg.SymFromAddrW.argtypes = [wt.HANDLE, ctypes.c_uint64, ctypes.POINTER(ctypes.c_uint64), ctypes.c_void_p]


class IMAGEHLP_LINEW64(ctypes.Structure):
    _fields_ = [('SizeOfStruct', wt.DWORD), ('Key', ctypes.c_void_p), ('LineNumber', wt.DWORD), ('FileName', ctypes.c_wchar_p), ('Address', ctypes.c_uint64)]


dbg.SymGetLineFromAddrW64.argtypes = [wt.HANDLE, ctypes.c_uint64, ctypes.POINTER(wt.DWORD), ctypes.POINTER(IMAGEHLP_LINEW64)]

# x64 CONTEXT: 16-byte aligned, ContextFlags at 0x30; Rsp 0x98, Rbp 0xA0, Rip 0xF8
buf = ctypes.create_string_buffer(1232 + 16)
caddr = (ctypes.addressof(buf) + 15) & ~15


k32.ReadProcessMemory.argtypes = [wt.HANDLE, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_size_t, ctypes.POINTER(ctypes.c_size_t)]
# stack copied per sample: the walk then runs on the copy with the thread already resumed
STACK_COPY = 256 * 1024
stack_buf = ctypes.create_string_buffer(STACK_COPY)


def snapshot():
    """With the thread suspended: its registers and the top of its stack, copied (~0.1 ms, versus
    several ms for a stack walk done while it waits). None if the context can't be read."""
    ctypes.memmove(caddr + 0x30, ctypes.byref(wt.DWORD(0x10000B)), 4)  # CONTEXT_FULL
    if not k32.GetThreadContext(target, ctypes.c_void_p(caddr)):
        return None
    ctx = ctypes.string_at(caddr, 1232)
    rsp = int.from_bytes(ctx[0x98:0xA0], 'little')
    # as much as is readable, page by page from rsp: the stack ends somewhere inside the copy size
    got = 0
    n = ctypes.c_size_t()
    while got < STACK_COPY:
        size = min(STACK_COPY - got, 0x1000 - (rsp + got) % 0x1000)
        if not k32.ReadProcessMemory(hp, rsp + got, ctypes.addressof(stack_buf) + got, size, ctypes.byref(n)) or n.value == 0:
            break
        got += n.value
    return ctx, rsp, stack_buf.raw[:got]


def walk(snap):
    """The return addresses of a snapshot's stack, walked by dbghelp after the thread resumed: reads
    inside the copied stack come from the copy, the rest (code, unwind data) from the process."""
    ctx, rsp, stack = snap
    ctypes.memmove(caddr, ctx, len(ctx))

    def read(_h, addr, out, size, nread):
        off = addr - rsp
        if 0 <= off and off + size <= len(stack):
            ctypes.memmove(out, stack[off:off + size], size)
            if nread:
                ctypes.cast(nread, ctypes.POINTER(wt.DWORD))[0] = size
            return True
        n = ctypes.c_size_t()
        ok = k32.ReadProcessMemory(hp, addr, out, size, ctypes.byref(n))
        if nread:
            ctypes.cast(nread, ctypes.POINTER(wt.DWORD))[0] = n.value
        return bool(ok)

    reader = READ_MEM(read)
    rip, rbp = (int.from_bytes(ctx[o:o + 8], 'little') for o in (0xF8, 0xA0))
    sf = STACKFRAME64()
    sf.AddrPC.Offset, sf.AddrPC.Mode = rip, 3
    sf.AddrStack.Offset, sf.AddrStack.Mode = rsp, 3
    sf.AddrFrame.Offset, sf.AddrFrame.Mode = rbp, 3
    pcs = []
    for _ in range(32):
        if not dbg.StackWalk64(0x8664, hp, target, ctypes.byref(sf), ctypes.c_void_p(caddr), reader, FTA, GMB, None) or not sf.AddrPC.Offset:
            break
        pcs.append(sf.AddrPC.Offset)
    return pcs


# PREAD_PROCESS_MEMORY_ROUTINE64(hProcess, qwBaseAddress, lpBuffer, nSize, lpNumberOfBytesRead)
READ_MEM = ctypes.WINFUNCTYPE(wt.BOOL, wt.HANDLE, ctypes.c_uint64, ctypes.c_void_p, wt.DWORD, ctypes.c_void_p)


# Every module's symbols now, while the thread runs: with deferred loads, the first stack walk
# loaded the exe's PDB with the thread suspended, stalling it for seconds, and the samples then
# caught it catching up (a backlog of landings and presents, not its usual work).
psapi = ctypes.WinDLL('psapi')
mods = (ctypes.c_uint64 * 1024)()
need = wt.DWORD()
t_load = time.perf_counter()
if psapi.EnumProcessModulesEx(hp, ctypes.byref(mods), ctypes.sizeof(mods), ctypes.byref(need), 3):
    for base in mods[:need.value // 8]:
        if base:
            si = SYMBOL_INFOW()
            si.SizeOfStruct = 88
            si.MaxNameLen = 1023
            dbg.SymFromAddrW(hp, base + 0x1000, ctypes.byref(ctypes.c_uint64()), ctypes.byref(si))
            dbg.SymFunctionTableAccess64(hp, base + 0x1000)
print(f'symbols loaded in {time.perf_counter() - t_load:.1f} s', file=sys.stderr)

stacks = []
t_next = time.perf_counter()
t_start = t_next
failed = 0
suspended = 0.0  # seconds the thread spent suspended for our walks
walks = []
while len(stacks) < a.samples:
    t0 = time.perf_counter()
    if k32.SuspendThread(target) != 0xFFFFFFFF:
        snap = snapshot()
        k32.ResumeThread(target)
        held = time.perf_counter() - t0
        stacks.append(walk(snap) if snap else [])
        suspended += held
        walks.append(held)
        failed = 0
        # never hold the thread more than --max-suspended of the time: a slow walk pushes the
        # next sample back instead of freezing the program being measured
        t_next = max(t_next, t0 + held / a.max_suspended - 1.0 / a.hz)
    else:
        # the thread (or the process) ended: report what was sampled instead of waiting forever
        failed += 1
        if failed >= a.hz:
            print(f'thread gone after {len(stacks)} samples', file=sys.stderr)
            break
    t_next += 1.0 / a.hz
    d = t_next - time.perf_counter()
    if d > 0:
        time.sleep(d)
wall = max(time.perf_counter() - t_start, 1e-9)
walks.sort()
# how much the measurement itself held the thread: above a few percent the profile shows a
# program slowed (and catching up) by the profiler, not its usual work
print(f'thread suspended {100 * suspended / wall:.1f}% of {wall:.1f} s; stack walk median {1e3 * walks[len(walks) // 2] if walks else 0:.2f} ms; '
      f'{len(stacks) / wall:.0f} samples/s', file=sys.stderr)

cache = {}


def sym(x):
    if x not in cache:
        si = SYMBOL_INFOW()
        si.SizeOfStruct = 88  # sizeof(SYMBOL_INFOW) without the name, as dbghelp expects
        si.MaxNameLen = 1023
        d = ctypes.c_uint64()
        cache[x] = si.Name if dbg.SymFromAddrW(hp, x, ctypes.byref(d), ctypes.byref(si)) else f'?{x:#x}'
    return cache[x]


def ours(n):
    return bool(re.search(r'runtime::|lma_app|lma_cli|guest::|gb\d\d::|7runtime|5guest', n))


leaf, pair, incl = collections.Counter(), collections.Counter(), collections.Counter()
for st in stacks:
    if not st:
        leaf['<no stack>'] += 1
        continue
    names = [sym(x)[:150] for x in st]
    leaf[names[0]] += 1
    caller = '(self)' if ours(names[0]) else next((n for n in names[1:] if ours(n)), '-')
    pair[(names[0], caller)] += 1
    for n in set(n for n in names if ours(n)):
        incl[n] += 1
T = max(len(stacks), 1)
print(f'{len(stacks)} samples of {a.thread!r} at {a.hz:g} Hz')
print('-- self (leaf) --')
for n, c in leaf.most_common(a.top):
    print(f'{100 * c / T:5.1f}%  {n}')
print('-- leaf <- first caller in our code --')
for (n, c_), c in pair.most_common(a.top):
    print(f'{100 * c / T:5.1f}%  {n}  <-  {c_}')
print('-- inclusive (our functions) --')
for n, c in incl.most_common(a.top):
    print(f'{100 * c / T:5.1f}%  {n}')

lines_cache = {}


def line(x):
    if x not in lines_cache:
        ln = IMAGEHLP_LINEW64()
        ln.SizeOfStruct = ctypes.sizeof(IMAGEHLP_LINEW64)
        d = wt.DWORD()
        if dbg.SymGetLineFromAddrW64(hp, x, ctypes.byref(d), ctypes.byref(ln)):
            f = (ln.FileName or '?').replace(chr(92), '/')
            lines_cache[x] = f"{f.split('/src/')[-1] if '/src/' in f else f.rsplit('/', 1)[-1]}:{ln.LineNumber}"
        else:
            lines_cache[x] = f'?{x:#x}'
    return lines_cache[x]


if a.lines:
    want = re.compile(a.lines)
    by_line = collections.Counter()
    hit = 0
    leaf_want = re.compile(a.leaf) if a.leaf else None
    for st in stacks:
        if leaf_want and not (st and leaf_want.search(sym(st[0]))):
            continue
        for i, x in enumerate(st):
            if want.search(sym(x)):
                # the matching frame's own pc, or the call site (return address - 1) when deeper
                by_line[line(x if i == 0 else x - 1)] += 1
                hit += 1
                break
    print(f'-- lines in /{a.lines}/{f" for leaves /{a.leaf}/" if a.leaf else ""} ({100 * hit / T:.1f}% of samples) --')
    for n, c in by_line.most_common(a.top):
        print(f'{100 * c / T:5.1f}%  {n}')
