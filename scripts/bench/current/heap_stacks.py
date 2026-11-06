#!/usr/bin/env python3
"""Summarize jemalloc heap_v2 stacks using ELF symbols without template stripping.

Uses the same Poisson sampling adjustment as jemalloc 5.3 jeprof. Release
symbols give function names, not source lines or allocation ownership.
"""
import argparse
import bisect
import json
import math
from pathlib import Path
import re
import subprocess

ap = argparse.ArgumentParser(description=__doc__)
ap.add_argument('binary')
ap.add_argument('heap')
ap.add_argument('--top', type=int, default=12)
args = ap.parse_args()
lines = Path(args.heap).read_text().splitlines()
period = int(lines[0].split('/')[1])
binary = str(Path(args.binary).resolve())
maps = [line.split() for line in lines if line.endswith(binary) and re.match(r'^[0-9a-f]+-', line)]
base = int(next(row[0].split('-')[0] for row in maps if int(row[2], 16) == 0), 16)
end = max(int(row[0].split('-')[1], 16) for row in maps)
symbols = []
for row in subprocess.check_output(['nm', '-n', '-S', '--defined-only', '--demangle', binary], text=True).splitlines():
    parts = row.split(None, 3)
    if len(parts) == 4 and parts[2] in ('t', 'T', 'W', 'w'):
        symbols.append((int(parts[0], 16), int(parts[1], 16), parts[3]))
symbols.sort()
starts = [row[0] for row in symbols]


def symbolize(address):
    if not base <= address < end:
        return None
    relative = address - base - 1  # recorded return address -> calling instruction
    index = bisect.bisect_right(starts, relative) - 1
    if index >= 0:
        start, size, name = symbols[index]
        if relative < start + size:
            return name
    return f'server+0x{relative:x}'


entries = []
stack = None
for line in lines:
    if line.startswith('@ '):
        stack = [int(value, 16) for value in line.split()[1:]]
    elif stack is not None and (match := re.match(r'\s*t\*: (\d+): (\d+)', line)):
        count, size = map(int, match.groups())
        if count and size:
            estimated = size / -math.expm1(-(size / count) / period)
            frames = [name for address in stack if (name := symbolize(address))]
            entries.append((estimated, count, size, frames))
        stack = None
entries.sort(reverse=True, key=lambda row: row[0])
result = {'heap': args.heap, 'binary': binary, 'estimated_inuse_mib': sum(row[0] for row in entries) / 2**20,
          'sample_period': period,
          'top_stacks': [{'estimated_mib': weight / 2**20, 'sampled_objects': count,
                          'sampled_bytes': size, 'frames': frames}
                         for weight, count, size, frames in entries[:args.top]]}
print(json.dumps(result, indent=2))
