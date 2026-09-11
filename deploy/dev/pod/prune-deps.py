#!/usr/bin/env python3
"""Keep only the newest build of each test/bin artifact under target/debug/deps.

Every code change leaves the previous copy of every test binary behind — ~800 MB each, a hundred
of them per build — and nothing in cargo ever removes them: 189 GB of them filled /work on
2026-09-10 and the linker died with SIGBUS. The newest per name is the one cargo will run; the
rest are dead weight. Libraries (.rlib/.rmeta/.so) are left alone: cargo's fingerprints reference
them across crates and a stale-looking one may still be live.

usage: prune-deps.py [target/debug/deps]
"""
import collections
import os
import re
import sys

root = sys.argv[1] if len(sys.argv) > 1 else "/work/target/debug/deps"
by = collections.defaultdict(list)
for f in os.listdir(root):
    m = re.match(r"^(.*)-([0-9a-f]{16})$", f)
    if not m:
        continue
    by[m.group(1)].append((os.stat(os.path.join(root, f)).st_mtime, m.group(2)))
freed = 0
for name, builds in by.items():
    builds.sort(reverse=True)
    for _, h in builds[1:]:
        for f in (f"{name}-{h}", f"{name}-{h}.d"):
            try:
                p = os.path.join(root, f)
                freed += os.stat(p).st_size
                os.remove(p)
            except FileNotFoundError:
                pass
print(f"deps: kept the newest of {len(by)} names, freed {freed / 1e9:.0f} GB")
