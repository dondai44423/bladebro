#!/usr/bin/env python3
"""Reject Linux release artifacts linked against newer-than-supported glibc."""
import re
import subprocess
import sys

if len(sys.argv) < 2:
    raise SystemExit('usage: linux-abi.py <linux-binary> ...')
for path in sys.argv[1:]:
    symbols = subprocess.run(['objdump', '-T', path], check=True, capture_output=True, text=True).stdout
    versions = {tuple(map(int, version.split('.'))) for version in re.findall(r'GLIBC_(\d+(?:\.\d+)+)', symbols)}
    if not versions:
        raise SystemExit(f'{path}: no GLIBC version requirements found')
    latest = max(versions)
    if latest > (2, 28):
        raise SystemExit(f'{path}: requires glibc {".".join(map(str,latest))}; release maximum is 2.28')
    print(f'PASS {path}: highest glibc requirement {".".join(map(str,latest))}')
