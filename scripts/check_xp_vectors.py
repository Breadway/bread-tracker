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
    for j, item in enumerate(c["items"]):
        if not isinstance(item, dict):
            sys.exit(f"{path}: case {i} ({c['name']}) item {j} is not an object")
        for key in ("id", "kind", "weekdays", "points", "active"):
            if key not in item:
                sys.exit(f"{path}: case {i} ({c['name']}) item {j} is missing {key!r}")
        # Optional provenance fields. Both implementations must parse them, and a typo would
        # otherwise be silently ignored by a lenient deserializer.
        created_by = item.get("created_by")
        if created_by is not None and (
            isinstance(created_by, bool) or not isinstance(created_by, int)
        ):
            sys.exit(f"{path}: case {i} ({c['name']}) item {j} created_by must be an int or null")
        created_on = item.get("created_on")
        if created_on is not None and not isinstance(created_on, str):
            sys.exit(f"{path}: case {i} ({c['name']}) item {j} created_on must be a string or null")
names = [c["name"] for c in cases]
if len(set(names)) != len(names):
    sys.exit(f"{path}: duplicate case names")
print(f"{path}: {len(cases)} cases ok")
