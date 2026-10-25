#!/usr/bin/env python3
"""Database evidence for the bread-tracker upgrade runbook (packaging/DEPLOY.md).

Standard library only, so it runs on the host with the python3 the runbook already uses. It
opens databases read-only and never writes to them.

  db-evidence.py show    DB
  db-evidence.py check   DB [--ledger-subset 1,3] [--ledger-exact 1,2,3,5,6]
  db-evidence.py compare FROZEN REPLICA

`show` prints the evidence as JSON: integrity_check, foreign_key_check, row counts, meta.seq,
the recorded migration versions, and a digest of every row of every data table.

`check` exits non-zero unless the database opens, passes integrity_check, has no foreign key
violations, and (when asked) its migration ledger fits the expected versions.

`compare` is the gate for the backup replica: it exits non-zero unless BOTH databases pass the
same checks and hold exactly the same data (identical digest, counts, meta.seq and ledger). A
missing, empty, unreadable, stale or different replica fails. File times are never used: a
fresh file name or modification time proves nothing about what is inside.
"""
import argparse
import hashlib
import json
import os
import sqlite3
import sys

# Not data: SQLx bookkeeping (compared separately as a version list) and SQLite internals.
SKIP_TABLES = {"_sqlx_migrations"}


class Bad(Exception):
    pass


def open_ro(path):
    if not os.path.isfile(path):
        raise Bad(f"{path}: no such file")
    if os.path.getsize(path) == 0:
        raise Bad(f"{path}: empty file")
    try:
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        con.execute("select count(*) from sqlite_master").fetchone()
    except sqlite3.Error as e:
        raise Bad(f"{path}: cannot open as a database: {e}")
    return con


def evidence(path):
    con = open_ro(path)
    try:
        tables = [
            r[0]
            for r in con.execute(
                "select name from sqlite_master where type = 'table' "
                "and name not like 'sqlite_%' order by name"
            )
            if r[0] not in SKIP_TABLES
        ]
        digest = hashlib.sha256()
        counts = {}
        for t in tables:
            cols = [r[1] for r in con.execute(f'pragma table_info("{t}")')]
            order = ", ".join(f'"{c}"' for c in cols)
            n = 0
            digest.update(f"table {t} {cols}\n".encode())
            for row in con.execute(f'select * from "{t}" order by {order}'):
                digest.update(repr(row).encode() + b"\n")
                n += 1
            counts[t] = n
        try:
            seq = con.execute("select val from meta where key = 'seq'").fetchone()
            seq = seq[0] if seq else None
        except sqlite3.Error:
            seq = None
        try:
            ledger = [
                r[0] for r in con.execute("select version from _sqlx_migrations order by version")
            ]
        except sqlite3.Error:
            ledger = []
        return {
            "file": os.path.basename(path),
            "integrity": con.execute("pragma integrity_check").fetchone()[0],
            "foreign_key_violations": len(con.execute("pragma foreign_key_check").fetchall()),
            "counts": counts,
            "meta_seq": seq,
            "ledger": ledger,
            "data_digest": digest.hexdigest(),
        }
    finally:
        con.close()


def problems(ev, subset=None, exact=None):
    out = []
    if ev["integrity"] != "ok":
        out.append(f"integrity_check is {ev['integrity']!r}, not 'ok'")
    if ev["foreign_key_violations"]:
        out.append(f"{ev['foreign_key_violations']} foreign key violation(s)")
    if not ev["counts"]:
        out.append("no data tables: not a bread-tracker database")
    if ev["meta_seq"] is None:
        out.append("meta.seq is missing: not a bread-tracker database")
    if subset is not None and not set(ev["ledger"]) <= set(subset):
        extra = sorted(set(ev["ledger"]) - set(subset))
        out.append(f"migration versions {extra} are not supported by the release being installed")
    if exact is not None and ev["ledger"] != sorted(exact):
        out.append(f"migration ledger is {ev['ledger']}, expected {sorted(exact)}")
    return out


def versions(text):
    return [int(v) for v in text.split(",") if v.strip()] if text is not None else None


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("show")
    s.add_argument("db")
    c = sub.add_parser("check")
    c.add_argument("db")
    c.add_argument("--ledger-subset")
    c.add_argument("--ledger-exact")
    p = sub.add_parser("compare")
    p.add_argument("frozen")
    p.add_argument("replica")
    a = ap.parse_args(argv)

    try:
        if a.cmd == "show":
            print(json.dumps(evidence(a.db), indent=2))
            return 0
        if a.cmd == "check":
            ev = evidence(a.db)
            print(json.dumps(ev, indent=2))
            bad = problems(ev, versions(a.ledger_subset), versions(a.ledger_exact))
            for b in bad:
                print(f"FAIL: {b}", file=sys.stderr)
            print("CHECK " + ("FAILED" if bad else "PASSED"))
            return 1 if bad else 0
        frozen, replica = evidence(a.frozen), evidence(a.replica)
        print("frozen :", json.dumps(frozen))
        print("replica:", json.dumps(replica))
        bad = [f"frozen copy: {b}" for b in problems(frozen)]
        bad += [f"replica: {b}" for b in problems(replica)]
        for key in ("counts", "meta_seq", "ledger", "data_digest"):
            if frozen[key] != replica[key]:
                bad.append(f"replica differs from the frozen copy in {key}")
        for b in bad:
            print(f"FAIL: {b}", file=sys.stderr)
        print("COMPARE " + ("FAILED: do not deploy" if bad else "PASSED: replica matches the frozen copy"))
        return 1 if bad else 0
    except Bad as e:
        print(f"FAIL: {e}", file=sys.stderr)
        print("CHECK FAILED" if a.cmd != "compare" else "COMPARE FAILED: do not deploy")
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
