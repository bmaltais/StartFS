#!/usr/bin/env python3
"""Build a tree of random files to copy into a StartFS mount, for repeatable burst tests.

Usage: make-tree.py DEST [--files N] [--big N] [--dirs N] [--depth N] [--seed N]

  --files N   small files, 8 KiB to 72 KiB each (default 4000)
  --big N     files of 1 MiB (default 200)
  --dirs N    directories the files are spread over (default 400)
  --depth N   deepest nesting of those directories (default 4)
  --seed N    same seed, same tree byte for byte; default is the clock, so every run is new data.
              Chunks are stored by content, so copying a tree that is already stored (or was
              deleted less than gc_retention_secs ago) sends almost nothing. Use a new seed.

Prints the seed, the number of files and directories and the total bytes.
"""
import argparse
import os
import random
import sys
import time


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("dest")
    p.add_argument("--files", type=int, default=4000)
    p.add_argument("--big", type=int, default=200)
    p.add_argument("--dirs", type=int, default=400)
    p.add_argument("--depth", type=int, default=4)
    p.add_argument("--seed", type=int, default=None)
    a = p.parse_args()
    if os.path.exists(a.dest) and os.listdir(a.dest):
        print(f"{a.dest} is not empty", file=sys.stderr)
        return 1
    seed = a.seed if a.seed is not None else int(time.time())
    rng = random.Random(seed)
    # Directories hang off a random earlier one, so the tree is lopsided like a real one.
    dirs = [a.dest]
    depth = {a.dest: 0}
    for i in range(max(a.dirs, 0)):
        parent = rng.choice(dirs)
        if depth[parent] >= a.depth:
            parent = a.dest
        d = os.path.join(parent, f"d{i}")
        depth[d] = depth[parent] + 1
        dirs.append(d)
    for d in dirs:
        os.makedirs(d, exist_ok=True)
    total = 0
    for i in range(a.files):
        n = rng.randint(8192, 73728)
        with open(os.path.join(rng.choice(dirs), f"f{i}"), "wb") as f:
            f.write(rng.randbytes(n))
        total += n
    for i in range(a.big):
        with open(os.path.join(rng.choice(dirs), f"b{i}"), "wb") as f:
            f.write(rng.randbytes(1 << 20))
        total += 1 << 20
    print(f"seed={seed} files={a.files + a.big} dirs={len(dirs) - 1} bytes={total}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
