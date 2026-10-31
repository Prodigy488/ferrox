#!/usr/bin/env python3
"""Regenerates crates/ferrox-hle/src/exports.rs from tools/xboxkrnl.exe.def.nxdk
(the nxdk project's kernel export list, SPDX CC0-1.0)."""
import re, sys, pathlib
root = pathlib.Path(__file__).resolve().parent.parent
rows = []
for line in open(root / 'tools/xboxkrnl.exe.def.nxdk'):
    m = re.match(r'\s+(@?)(\w+?)(?:@(\d+))?\s+@\s*(\d+)\s+NONAME(\s+DATA)?', line)
    if not m:
        continue
    fast, name, n, ordn, data = m.groups()
    if data: conv, n = 'Data', 0
    elif fast: conv = 'Fast'
    elif n is None: conv, n = 'Cdecl', 0
    else: conv = 'Std'
    rows.append((int(ordn), name, int(n or 0), conv))
rows.sort()
path = root / 'crates/ferrox-hle/src/exports.rs'
src = path.read_text()
head, rest = src.split('pub static EXPORTS: &[ExportInfo] = &[\n', 1)
tail = rest[rest.index('];\n'):]
body = ''.join(f'    ExportInfo {{ ordinal: {o}, name: "{n}", arg_bytes: {a}, conv: Conv::{c} }},\n' for o, n, a, c in rows)
path.write_text(head + 'pub static EXPORTS: &[ExportInfo] = &[\n' + body + tail)
print(len(rows), 'exports')
