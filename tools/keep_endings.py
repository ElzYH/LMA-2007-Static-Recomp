"""Restore a file's original line endings after an editor normalised a mixed CRLF/LF file.
usage: keep_endings.py <repo dir> <base rev> <path in repo>
Lines whose text is unchanged from <base rev> get the base's exact bytes (their old ending); new or
changed lines take the ending of the base line they replace, else of the base line before them."""
import difflib, subprocess, sys

repo, base, path = sys.argv[1:4]
old = subprocess.run(['git', '-C', repo, 'show', f'{base}:{path}'], capture_output=True, check=True).stdout.splitlines(keepends=True)
cur = open(f'{repo}/{path}', 'rb').read().splitlines(keepends=True)
key = lambda l: l.rstrip(b'\r\n')
ending = lambda l: l[len(key(l)):]
last_ending = lambda out: next((ending(l) for l in reversed(out) if ending(l)), b'\n')
out = []
sm = difflib.SequenceMatcher(None, [key(l) for l in old], [key(l) for l in cur], autojunk=False)
for op, i1, i2, j1, j2 in sm.get_opcodes():
    if op == 'equal':
        for k, l in enumerate(old[i1:i2]):  # a last line without newline may get one when lines follow
            out.append(l if ending(l) or not ending(cur[j1 + k]) else key(l) + last_ending(out))
    elif op in ('replace', 'insert'):
        for k, l in enumerate(cur[j1:j2]):
            ref = old[i1 + k] if op == 'replace' and i1 + k < i2 else (old[i1 - 1] if i1 > 0 else old[0] if old else b'\n')
            e = ending(l) and (ending(ref) or last_ending(out))
            out.append(key(l) + e)
    # delete: drop the base lines
data = b''.join(out)
open(f'{repo}/{path}', 'wb').write(data)
print(path, 'crlf', data.count(b'\r\n'), 'lf', data.count(b'\n') - data.count(b'\r\n'))
