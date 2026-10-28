#!/usr/bin/env python3
"""Database evidence for the bread-tracker upgrade runbook (packaging/DEPLOY.md).

Standard library only, so it runs on the host with the python3 the runbook already uses. It
never writes to the databases it is given: it copies the database file and its -wal / -shm
sidecars (if present) into a private temporary directory and reads the copy, so the originals
and their sidecars stay byte-identical.

  db-evidence.py show    DB
  db-evidence.py check   DB [--ledger-subset 1,3] [--ledger-exact 1,2,3,5,6]
  db-evidence.py compare FROZEN REPLICA

Every command takes --migrations DIR (default: the migrations directory of this checkout).
Migration files there are the immutable source of truth for the checksums SQLx records.

`show` prints the evidence as JSON.

`check` exits non-zero unless the database opens, passes integrity_check, has no foreign key
violations, has a valid migration ledger and (when asked) a ledger that fits the expected
versions.

`compare` is the gate for the backup replica: it exits non-zero unless BOTH databases pass the
same checks and hold exactly the same data and the same migration ledger. A missing, empty,
unreadable, stale or different replica fails. File times are never used: a fresh file name or
modification time proves nothing about what is inside.

The migration ledger (SQLx's _sqlx_migrations table) is validated, not just listed:
  * the table must exist, have the columns SQLx writes, and hold at least one row, including
    version 1 (every bread-tracker database records its initial migration);
  * every row needs an integer version (no duplicates), success = 1 (a failed migration is
    "dirty" and SQLx refuses it), and a 48 byte SHA-384 checksum;
  * a version that has a file in the migrations directory must carry exactly that file's
    SHA-384, so a ledger written for different migration bytes is rejected;
  * two ledgers are equal when their (version, checksum, success) rows are equal. The
    description, installed_on and execution_time columns are bookkeeping that SQLx itself does
    not compare, and they legitimately differ between a live database and its replica.

Data is compared as a multiset of rows per table (a digest of the sorted, typed serialization
of every row), so row order, collations and storage order cannot change the result.
"""
import argparse
import hashlib
import json
import os
import shutil
import sqlite3
import sys
import tempfile
from pathlib import Path

# Not data: SQLx bookkeeping, validated and compared separately as the ledger.
SKIP_TABLES = {"_sqlx_migrations"}
LEDGER_NEEDED_COLUMNS = {"version", "description", "installed_on", "success", "checksum", "execution_time"}
SHA384_BYTES = 48


class Bad(Exception):
    pass


def default_migrations_dir():
    return Path(__file__).resolve().parent.parent / "migrations"


def known_checksums(directory):
    """version -> SHA-384 hex of the shipped migration file NNNN_name.sql."""
    out = {}
    d = Path(directory)
    if not d.is_dir():
        raise Bad(f"{d}: migrations directory not found (use --migrations)")
    for f in sorted(d.glob("*.sql")):
        head = f.name.split("_", 1)[0]
        if head.isdigit():
            out[int(head)] = hashlib.sha384(f.read_bytes()).hexdigest()
    if not out:
        raise Bad(f"{d}: no migration files found")
    return out


def q(identifier):
    return '"' + identifier.replace('"', '""') + '"'


def open_copy(path, tmp):
    """Open a private copy of the database and its sidecars, read-only."""
    src = Path(path)
    if not src.is_file():
        raise Bad(f"{path}: no such file")
    if src.stat().st_size == 0:
        raise Bad(f"{path}: empty file")
    dst = Path(tmp) / "db.sqlite"
    shutil.copyfile(src, dst)
    for suffix in ("-wal", "-shm"):
        side = Path(str(src) + suffix)
        if side.is_file():
            shutil.copyfile(side, Path(str(dst) + suffix))
    try:
        con = sqlite3.connect(dst.as_uri() + "?mode=ro", uri=True)
        con.execute("select count(*) from sqlite_master").fetchone()
    except sqlite3.Error as e:
        raise Bad(f"{path}: cannot open as a database: {e}")
    return con


def read_ledger(con):
    """Return (rows, problems). rows are {version, checksum, success}, sorted by version."""
    cols = {r[1] for r in con.execute("pragma table_info(_sqlx_migrations)")}
    if not cols:
        return [], ["migration ledger (_sqlx_migrations) is missing"]
    missing = LEDGER_NEEDED_COLUMNS - cols
    if missing:
        return [], [f"migration ledger lacks column(s) {sorted(missing)}"]
    problems, rows, seen = [], [], set()
    for version, success, checksum in con.execute(
        "select version, success, checksum from _sqlx_migrations order by version, rowid"
    ):
        if not isinstance(version, int) or isinstance(version, bool) or version < 1:
            problems.append(f"migration ledger has an invalid version {version!r}")
            continue
        if version in seen:
            problems.append(f"migration {version} is recorded more than once")
        seen.add(version)
        if success not in (0, 1) or isinstance(success, bool):
            problems.append(f"migration {version} has a malformed success flag {success!r}")
        elif success != 1:
            problems.append(f"migration {version} is recorded as failed (success = {success})")
        if not isinstance(checksum, (bytes, bytearray)):
            problems.append(f"migration {version} has no checksum")
            checksum_hex = None
        elif len(checksum) != SHA384_BYTES:
            problems.append(f"migration {version} checksum is {len(checksum)} bytes, not {SHA384_BYTES}")
            checksum_hex = bytes(checksum).hex()
        else:
            checksum_hex = bytes(checksum).hex()
        rows.append({"version": version, "checksum": checksum_hex, "success": success})
    if not rows:
        problems.append("migration ledger is empty")
    elif 1 not in seen:
        problems.append("migration 1 (the initial schema) is not recorded")
    return rows, problems


def evidence(path, known):
    with tempfile.TemporaryDirectory(prefix="db-evidence-") as tmp:
        con = open_copy(path, tmp)
        try:
            return gather(con, path, known)
        except sqlite3.Error as e:
            raise Bad(f"{path}: cannot be read: {e}")
        finally:
            con.close()


def gather(con, path, known):
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
        cols = [r[1] for r in con.execute(f"pragma table_info({q(t)})")]
        digest.update(f"table {t} {cols}\n".encode())
        # Sorted in Python on the typed serialization: a SQL ORDER BY is not total when a
        # collation treats distinct values as equal, which would make the digest depend on
        # physical row order.
        rows = sorted(repr(row) for row in con.execute(f"select * from {q(t)}"))
        for r in rows:
            digest.update(r.encode() + b"\n")
        counts[t] = len(rows)
    try:
        seq = con.execute("select val from meta where key = 'seq'").fetchone()
        seq = seq[0] if seq else None
    except sqlite3.Error:
        seq = None
    ledger, ledger_problems = read_ledger(con)
    for row in ledger:
        shipped = known.get(row["version"])
        if shipped is not None and row["checksum"] is not None and row["checksum"] != shipped:
            ledger_problems.append(
                f"migration {row['version']} checksum {row['checksum'][:16]}... is not the shipped "
                f"file's {shipped[:16]}..."
            )
    return {
        "file": os.path.basename(path),
        "integrity": con.execute("pragma integrity_check").fetchone()[0],
        "foreign_key_violations": len(con.execute("pragma foreign_key_check").fetchall()),
        "counts": counts,
        "meta_seq": seq,
        "ledger": ledger,
        "ledger_problems": ledger_problems,
        "data_digest": digest.hexdigest(),
    }


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
    out.extend(ev["ledger_problems"])
    versions = [r["version"] for r in ev["ledger"]]
    if subset is not None and not set(versions) <= set(subset):
        extra = sorted(set(versions) - set(subset))
        out.append(f"migration versions {extra} are not supported by the release being installed")
    if exact is not None and versions != sorted(exact):
        out.append(f"migration ledger is {versions}, expected {sorted(exact)}")
    return out


def versions_arg(text):
    return [int(v) for v in text.split(",") if v.strip()] if text is not None else None


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--migrations", default=None, help="directory of shipped migration files")
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
    for sp in (s, c, p):
        sp.add_argument("--migrations", default=argparse.SUPPRESS)
    a = ap.parse_args(argv)

    try:
        known = known_checksums(a.migrations or default_migrations_dir())
        if a.cmd == "show":
            print(json.dumps(evidence(a.db, known), indent=2))
            return 0
        if a.cmd == "check":
            ev = evidence(a.db, known)
            print(json.dumps(ev, indent=2))
            bad = problems(ev, versions_arg(a.ledger_subset), versions_arg(a.ledger_exact))
            for b in bad:
                print(f"FAIL: {b}", file=sys.stderr)
            print("CHECK " + ("FAILED" if bad else "PASSED"))
            return 1 if bad else 0
        frozen, replica = evidence(a.frozen, known), evidence(a.replica, known)
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
