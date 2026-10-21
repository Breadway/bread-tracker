#!/usr/bin/env python3
"""Sanity-check spec/xp_vectors.json before the two implementations are run against it.

server/src/xp.rs and app/lib/rules.dart both read this file, so a malformed or empty file
would make a test vacuously pass. Fails when the file is not a non-empty list of cases that
each have a name, a date, items, completions and an expectation.
"""
import json
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "spec/xp_vectors.json"
cases = json.load(open(path))
if not isinstance(cases, list) or not cases:
    sys.exit(f"{path}: expected a non-empty list of cases")
for i, c in enumerate(cases):
    for key in ("name", "today", "items", "completions", "expect"):
        if key not in c:
            sys.exit(f"{path}: case {i} ({c.get('name', '?')}) is missing {key!r}")
    for key in ("xp", "level", "perfect_days", "streaks"):
        if key not in c["expect"]:
            sys.exit(f"{path}: case {i} ({c['name']}) expect is missing {key!r}")
names = [c["name"] for c in cases]
if len(set(names)) != len(names):
    sys.exit(f"{path}: duplicate case names")
print(f"{path}: {len(cases)} cases ok")
