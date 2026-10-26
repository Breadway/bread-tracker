#!/usr/bin/env bash
# Tests for scripts/db-evidence.py and scripts/backup-gate.sh, the gate behind the upgrade
# runbook in packaging/DEPLOY.md. Builds small real SQLite files; needs only bash and python3.
#
#   scripts/tests/db-evidence.sh
#
# To prove a test is not a false positive, point it at an older helper or runbook and expect
# failures:  TOOL=/path/old/db-evidence.py DOC=/path/old/DEPLOY.md GATE=/path/old/gate scripts/tests/db-evidence.sh
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
repo=$(cd "$here/.." && pwd)
tool=${TOOL:-$here/db-evidence.py}
doc=${DOC:-$repo/packaging/DEPLOY.md}
gate=${GATE:-$here/backup-gate.sh}
migrations=$repo/migrations
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

failures=0
fail() { echo "FAILED: $*" >&2; failures=$((failures + 1)); }
# `tool` accepts --migrations only on the new helper; the old one does not know it.
t() { python3 "$tool" "$@"; }
tm() { python3 "$tool" "$@" --migrations "$migrations" 2>/dev/null || python3 "$tool" "$@"; }


passes() { local name=$1; shift; "$@" >"$tmp/out" 2>"$tmp/err" || { fail "$name should pass (exit $?)"; return 1; }; }
stops() {
  local name=$1 want=$2; shift 2
  if "$@" >"$tmp/out" 2>"$tmp/err"; then fail "$name should stop but exited 0"; return 1; fi
  grep -q -- "$want" "$tmp/out" "$tmp/err" || { fail "$name stopped without '$want': $(cat "$tmp/out" "$tmp/err" | tail -3 | tr '\n' ' ')"; return 1; }
}

# ---- fixture: a real upgraded database, ledger recorded exactly as SQLx records it
build() {  # build DB 'versions...' : apply the repository's migrations and record them
  python3 - "$1" "$migrations" "${@:2}" <<'PY'
import hashlib, sqlite3, sys, glob, os
db, mig, *versions = sys.argv[1:]
c = sqlite3.connect(db)
c.execute("create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null)")
files = {int(os.path.basename(f).split('_')[0]): f for f in glob.glob(f"{mig}/*.sql")}
for v in map(int, versions):
    sql = open(files[v], "rb").read()
    c.executescript(sql.decode())
    c.execute("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (?,?,1,?,?)", (v, os.path.basename(files[v]), hashlib.sha384(sql).digest(), 12345 + v))
c.executescript("""
insert into items (user_id,id,kind,name,points,updated_at,seq) values (1,'i1','chore','Dishes',10,1,1),(1,'i2','habit','Read',5,1,2);
insert into completions (user_id,item_id,date,completed_at,updated_at,seq) values (1,'i1','2026-10-20',1,1,3);
insert into devices (id,user_id,name,token_hash,created_at,last_seen) values ('d1',1,'Pixel','h1',1,1);
update meta set val = 3 where key = 'seq';
""")
c.commit(); c.close()
PY
}
sql() { python3 - "$@" <<'PY'
import sqlite3, sys
db, *stmts = sys.argv[1:]
c = sqlite3.connect(db)
for s in stmts:
    c.execute(s)
c.commit(); c.close()
PY
}
clone() { cp "$1" "$2"; }

build "$tmp/live.db" 1 2 3 5 6
clone "$tmp/live.db" "$tmp/frozen.db"; clone "$tmp/live.db" "$tmp/replica.db"
SUBSET=1,2,3,5,6

# ======== positives (must hold before and after the fix)
passes "check good" tm check "$tmp/frozen.db" --ledger-subset $SUBSET && grep -q "CHECK PASSED" "$tmp/out" || fail "check prints CHECK PASSED"
passes "check exact" tm check "$tmp/frozen.db" --ledger-exact $SUBSET
stops "ledger too small" "expected" tm check "$tmp/frozen.db" --ledger-exact 1,2,3,6
stops "unsupported version" "not supported by the release" tm check "$tmp/frozen.db" --ledger-subset 1,2,3
passes "compare identical" tm compare "$tmp/frozen.db" "$tmp/replica.db" && grep -q "COMPARE PASSED" "$tmp/out" || fail "compare prints COMPARE PASSED"
# ordinary legacy ledgers: 1 only and 1+3 are valid (a database from before the upgrade)
python3 - "$tmp/legacy1.db" "$migrations" <<'PY'
import hashlib, sqlite3, sys
db, mig = sys.argv[1:]
c = sqlite3.connect(db)
c.execute("create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null)")
sql = open(f"{mig}/0001_init.sql", "rb").read(); c.executescript(sql.decode())
c.execute("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (1,'init',1,?,0)", (hashlib.sha384(sql).digest(),))
c.commit()
PY
[ -s "$tmp/legacy1.db" ] || fail "legacy 1-only fixture was not built"
passes "legacy 1 only" tm check "$tmp/legacy1.db" --ledger-subset $SUBSET
python3 - "$tmp/legacy13.db" "$migrations" <<'PY'
import hashlib, sqlite3, sys, os
db, mig = sys.argv[1:]
c = sqlite3.connect(db)
c.execute("create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null)")
for v, f in [(1, "0001_init"), (3, "0003_hygiene_indexes")]:
    sql = open(f"{mig}/{f}.sql", "rb").read(); c.executescript(sql.decode())
    c.execute("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (?,?,1,?,0)", (v, f, hashlib.sha384(sql).digest()))
c.commit()
PY
passes "legacy 1+3" tm check "$tmp/legacy13.db" --ledger-subset $SUBSET
passes "legacy 1+3 compares to itself" tm compare "$tmp/legacy13.db" "$tmp/legacy13.db"
# bookkeeping columns legitimately differ between a live database and its replica
clone "$tmp/frozen.db" "$tmp/bookkeeping.db"
sql "$tmp/bookkeeping.db" "update _sqlx_migrations set installed_on = '2001-01-01 00:00:00', execution_time = 99999999, description = 'renamed'"
passes "installed_on / execution_time / description may differ" tm compare "$tmp/frozen.db" "$tmp/bookkeeping.db"
# file times never matter
touch -d '2001-01-01' "$tmp/replica.db"
passes "old mtime, same data" tm compare "$tmp/frozen.db" "$tmp/replica.db"
clone "$tmp/frozen.db" "$tmp/newer.db"; touch "$tmp/newer.db"; sql "$tmp/newer.db" "update items set name = 'Washing up' where id = 'i1'"
stops "fresh mtime, different row content" "data_digest" tm compare "$tmp/frozen.db" "$tmp/newer.db"
grep -q "COMPARE FAILED: do not deploy" "$tmp/out" || fail "compare prints COMPARE FAILED"
# a recorded 0004 is refused by the release check
clone "$tmp/frozen.db" "$tmp/with4.db"
sql "$tmp/with4.db" "insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (4,'pair',1,zeroblob(48),0)"
stops "recorded 0004" "\[4\] are not supported" tm check "$tmp/with4.db" --ledger-subset $SUBSET
# stale replica
clone "$tmp/frozen.db" "$tmp/stale.db"
sql "$tmp/frozen.db" "insert into items (user_id,id,kind,name,points,updated_at,seq) values (1,'i3','chore','New',1,1,4)" "update meta set val = 4 where key = 'seq'"
stops "stale replica: seq" "meta_seq" tm compare "$tmp/frozen.db" "$tmp/stale.db"
clone "$tmp/live.db" "$tmp/frozen.db"
# missing, empty, garbage
stops "missing replica" "no such file" tm compare "$tmp/frozen.db" "$tmp/nope.db"
: > "$tmp/empty.db"; stops "empty replica" "empty file" tm compare "$tmp/frozen.db" "$tmp/empty.db"
head -c 4096 /dev/urandom > "$tmp/garbage.db"
stops "garbage replica" "cannot\|not a bread-tracker\|integrity" tm compare "$tmp/frozen.db" "$tmp/garbage.db"
clone "$tmp/live.db" "$tmp/fk.db"; sql "$tmp/fk.db" "insert into items (user_id,id,kind,name,points,updated_at,seq) values (99,'orphan','chore','x',1,1,9)" 2>/dev/null
python3 - "$tmp/fk.db" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute("pragma foreign_keys = off")
c.execute("insert or ignore into items (user_id,id,kind,name,points,updated_at,seq) values (99,'orphan','chore','x',1,1,9)"); c.commit()
PY
stops "foreign key violation" "foreign key violation" tm compare "$tmp/fk.db" "$tmp/fk.db"

# ======== NEW (review 7241): the ledger is validated and compared as a ledger
clone "$tmp/live.db" "$tmp/ck.db";  sql "$tmp/ck.db" "update _sqlx_migrations set checksum = x'00' where version = 1"
stops "LEDGER checksum changed to 00 is rejected by compare" "checksum" tm compare "$tmp/frozen.db" "$tmp/ck.db"
stops "LEDGER malformed checksum is rejected by check" "checksum" tm check "$tmp/ck.db" --ledger-subset $SUBSET
clone "$tmp/live.db" "$tmp/ck2.db"; sql "$tmp/ck2.db" "update _sqlx_migrations set checksum = zeroblob(48) where version = 1"
stops "LEDGER well-formed but wrong checksum (not the shipped file) is rejected" "not the shipped" tm check "$tmp/ck2.db" --ledger-subset $SUBSET
stops "LEDGER wrong checksum differs from frozen in compare" "ledger" tm compare "$tmp/frozen.db" "$tmp/ck2.db"
clone "$tmp/live.db" "$tmp/ok0.db"; sql "$tmp/ok0.db" "update _sqlx_migrations set success = 0 where version = 6"
stops "LEDGER failed (success = 0) migration is rejected by compare" "failed" tm compare "$tmp/frozen.db" "$tmp/ok0.db"
stops "LEDGER failed (success = 0) migration is rejected by check" "failed" tm check "$tmp/ok0.db" --ledger-subset $SUBSET
clone "$tmp/live.db" "$tmp/noled.db"; sql "$tmp/noled.db" "drop table _sqlx_migrations"
stops "LEDGER missing table is rejected by check --ledger-subset" "missing" tm check "$tmp/noled.db" --ledger-subset $SUBSET
stops "LEDGER missing table is rejected by check without options" "missing" tm check "$tmp/noled.db"
stops "LEDGER missing table is rejected by compare" "missing" tm compare "$tmp/frozen.db" "$tmp/noled.db"
clone "$tmp/live.db" "$tmp/emptyled.db"; sql "$tmp/emptyled.db" "delete from _sqlx_migrations"
stops "LEDGER empty is rejected" "empty\|not recorded" tm check "$tmp/emptyled.db" --ledger-subset $SUBSET
clone "$tmp/live.db" "$tmp/no1.db"; sql "$tmp/no1.db" "delete from _sqlx_migrations where version = 1"
stops "LEDGER without migration 1 is rejected" "migration 1" tm check "$tmp/no1.db" --ledger-subset $SUBSET
python3 - "$tmp/live.db" "$tmp/dup.db" <<'PY'
import sqlite3, sys
src, dst = sys.argv[1:]
sqlite3.connect(src).backup(sqlite3.connect(dst))
c = sqlite3.connect(dst)
# SQLx declares version as the primary key, so a duplicate can only exist in a table written
# some other way: rebuild the ledger without the key.
rows = c.execute("select version, description, installed_on, success, checksum, execution_time from _sqlx_migrations").fetchall()
c.execute("drop table _sqlx_migrations")
c.execute("create table _sqlx_migrations (version bigint, description text, installed_on timestamp, success boolean, checksum blob, execution_time bigint)")
c.executemany("insert into _sqlx_migrations values (?,?,?,?,?,?)", rows + [rows[0]])
c.commit()
PY
stops "LEDGER duplicate version is rejected" "more than once" tm check "$tmp/dup.db" --ledger-subset $SUBSET
python3 - "$tmp/live.db" "$tmp/shape.db" <<'PY'
import sqlite3, sys
src, dst = sys.argv[1:]
sqlite3.connect(src).backup(sqlite3.connect(dst))
c = sqlite3.connect(dst)
c.execute("drop table _sqlx_migrations"); c.execute("create table _sqlx_migrations (version bigint primary key)")
c.execute("insert into _sqlx_migrations values (1)"); c.commit()
PY
stops "LEDGER without the columns SQLx writes is rejected" "lacks column" tm check "$tmp/shape.db" --ledger-subset $SUBSET
clone "$tmp/live.db" "$tmp/gone.db"; sql "$tmp/gone.db" "delete from _sqlx_migrations where version = 6"
stops "LEDGER with a migration missing differs in compare" "ledger" tm compare "$tmp/frozen.db" "$tmp/gone.db"
# row-order independence under a case-insensitive collation (reviewer probe)
clone "$tmp/live.db" "$tmp/ca.db"; clone "$tmp/live.db" "$tmp/cb.db"
python3 - "$tmp/ca.db" "$tmp/cb.db" <<'PY'
import sqlite3, sys
for dest, rows in [(sys.argv[1], ["Alex", "alex", "Älex", "älex"]), (sys.argv[2], ["älex", "Älex", "alex", "Alex"])]:
    c = sqlite3.connect(dest); c.execute("create table probe_values (value text collate nocase)")
    c.executemany("insert into probe_values values (?)", [(x,) for x in rows]); c.commit()
PY
passes "same rows in reverse order digest equally (collation ties)" tm compare "$tmp/ca.db" "$tmp/cb.db"

# ======== read-only: nothing of the original changes, WAL fixture included
# A WAL file with unmerged content exists only while a connection holds the database open, so
# a helper process keeps one open (autocheckpoint off) while the tool runs against the files.
python3 - "$tmp/live.db" "$tmp/wal.db" "$tmp/wal.ready" "$tmp/wal.stop" <<'PY' &
import os, shutil, sqlite3, sys, time
src, dst, ready, stop = sys.argv[1:]
shutil.copy(src, dst)
c = sqlite3.connect(dst, isolation_level=None)
c.execute("pragma journal_mode=wal"); c.execute("pragma wal_autocheckpoint=0")
c.execute("insert into items (user_id,id,kind,name,points,updated_at,seq) values (1,'w1','chore','InWal',1,1,7)")
c.execute("update meta set val = 7 where key = 'seq'")
open(ready, "w").close()
while not os.path.exists(stop):
    time.sleep(0.05)
c.close()
PY
walpid=$!
for _ in $(seq 1 100); do [ -f "$tmp/wal.ready" ] && break; sleep 0.05; done
if [ -s "$tmp/wal.db-wal" ]; then
  (cd "$tmp" && sha256sum wal.db wal.db-wal wal.db-shm) >"$tmp/walsum"
  tm check "$tmp/wal.db" >/dev/null 2>&1
  tm show "$tmp/wal.db" >"$tmp/wal.show" 2>&1
  (cd "$tmp" && sha256sum -c walsum >/dev/null 2>&1) || fail "tool changed the main file or its -wal/-shm sidecars"
  grep -q '"meta_seq": 7' "$tmp/wal.show" || fail "evidence must include the unmerged -wal content (seq 7)"
else
  fail "could not build an active WAL fixture"
fi
touch "$tmp/wal.stop"; wait $walpid 2>/dev/null

# ======== the executable gate: extract the REAL block from DEPLOY.md, follow it with a sentinel
# A fake docker on PATH stands in for the Litestream container; restore behaviour is switched
# by files, so the same documented command runs against success, restore failure, checker
# failure and a stale replica.
bin=$tmp/fakebin; mkdir -p "$bin"
cat >"$bin/docker" <<'FAKE'
#!/usr/bin/env bash
# fake `docker run ... restore -o /o/replica.db file:///b/litestream`
mode=$(cat "$FAKE_MODE")
out=""; src=""
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  [ "${args[i]}" = "-v" ] && case "${args[i+1]}" in *:/o) out=${args[i+1]%:/o};; *:/b:ro) src=${args[i+1]%:/b:ro};; esac
done
case "$mode" in
  restore-ok)      cp "$FAKE_REPLICA" "$out/replica.db";;
  restore-fail)    echo "ERROR no matching backups found" >&2; exit 1;;
  restore-empty)   : > "$out/replica.db";;
  restore-garbage) head -c 4096 /dev/urandom > "$out/replica.db";;
esac
FAKE
chmod +x "$bin/docker"
mkdir -p "$tmp/backups"

# the shell block between the step 3 heading and "Proceed to step 4", exactly as printed
python3 - "$doc" "$tmp/gate-block.sh" <<'PY'
import re, sys
doc = open(sys.argv[1]).read()
a = doc.index("3. **Mandatory backup gate")
b = doc.index("4. **Name the first user and deploy.**")
step = doc[a:b]
blocks = re.findall(r"```sh\n(.*?)```", step, re.S)
open(sys.argv[2], "w").write("\n".join(b.replace("\n   ", "\n").lstrip(" ") for b in blocks))
PY
[ -s "$tmp/gate-block.sh" ] || fail "no shell block found in step 3 of the runbook"

run_gate() {  # run_gate MODE REPLICA ; prints SENTINEL-DEPLOYED if the line after the gate ran
  local mode=$1 replica=$2
  echo "$mode" >"$tmp/mode"
  ( cd "$repo" && COPY=$tmp/copydir && mkdir -p "$COPY" && cp "$tmp/frozen.db" "$COPY/bread-tracker.db" &&
    export COPY BREADTRACKER_BACKUP_DIR="$tmp/backups" FAKE_MODE="$tmp/mode" FAKE_REPLICA="$replica" PATH="$bin:$PATH" &&
    # the documented set -e script: the gate block, then the deployment command (a sentinel)
    bash -e -c "$(cat "$tmp/gate-block.sh")
echo SENTINEL-DEPLOYED" ) >"$tmp/gate.out" 2>&1
  echo $? >"$tmp/gate.rc"
}
gate_must_stop() {
  local name=$1; shift
  run_gate "$@"
  if grep -q SENTINEL-DEPLOYED "$tmp/gate.out"; then fail "$name: the deployment sentinel ran after a failed gate"; return; fi
  [ "$(cat "$tmp/gate.rc")" != 0 ] || fail "$name: the gate block exited 0"
  grep -q "BACKUP GATE FAILED: do not deploy" "$tmp/gate.out" || fail "$name: no FAILED message"
  grep -q "BACKUP GATE PASSED" "$tmp/gate.out" && fail "$name: printed PASSED"
}
# success path: passes, exits 0, sentinel runs
run_gate restore-ok "$tmp/frozen.db"
grep -q "BACKUP GATE PASSED" "$tmp/gate.out" || fail "gate success: no PASSED ($(tail -3 "$tmp/gate.out" | tr '\n' ' '))"
[ "$(cat "$tmp/gate.rc")" = 0 ] || fail "gate success: exit $(cat "$tmp/gate.rc")"
grep -q SENTINEL-DEPLOYED "$tmp/gate.out" || fail "gate success: the deploy line did not run after a passing gate"
# failure paths: the sentinel must NEVER run
gate_must_stop "restore fails" restore-fail "$tmp/frozen.db"
gate_must_stop "restore leaves an empty file" restore-empty "$tmp/frozen.db"
gate_must_stop "restore leaves garbage" restore-garbage "$tmp/frozen.db"
clone "$tmp/frozen.db" "$tmp/gate-stale.db"; sql "$tmp/gate-stale.db" "update items set name = 'stale' where id = 'i1'"
gate_must_stop "restored replica differs (stale)" restore-ok "$tmp/gate-stale.db"
clone "$tmp/frozen.db" "$tmp/gate-ck.db"; sql "$tmp/gate-ck.db" "update _sqlx_migrations set checksum = x'00' where version = 1"
gate_must_stop "restored replica has a corrupted ledger checksum" restore-ok "$tmp/gate-ck.db"
clone "$tmp/frozen.db" "$tmp/gate-ok0.db"; sql "$tmp/gate-ok0.db" "update _sqlx_migrations set success = 0 where version = 6"
gate_must_stop "restored replica has a failed migration" restore-ok "$tmp/gate-ok0.db"
clone "$tmp/frozen.db" "$tmp/gate-noled.db"; sql "$tmp/gate-noled.db" "drop table _sqlx_migrations"
gate_must_stop "restored replica has no ledger" restore-ok "$tmp/gate-noled.db"
# missing frozen copy and missing backup directory fail closed too
rm -rf "$tmp/backups-gone"
( cd "$repo" && PATH="$bin:$PATH" bash -e -c "$gate \"$tmp/nope.db\" \"$tmp/backups\"
echo SENTINEL-DEPLOYED" ) >"$tmp/gate.out" 2>&1; rc=$?
{ [ "$rc" != 0 ] && ! grep -q SENTINEL-DEPLOYED "$tmp/gate.out"; } || fail "missing frozen copy must stop before the sentinel"
( cd "$repo" && PATH="$bin:$PATH" bash -e -c "$gate \"$tmp/frozen.db\" \"$tmp/backups-gone\"
echo SENTINEL-DEPLOYED" ) >"$tmp/gate.out" 2>&1; rc=$?
{ [ "$rc" != 0 ] && ! grep -q SENTINEL-DEPLOYED "$tmp/gate.out"; } || fail "missing backup directory must stop before the sentinel"
# the original defect in isolation: a bare if/else prints FAILED but exits 0
bash -c 'if false && true; then echo PASSED; else echo "BACKUP GATE FAILED: do not deploy"; fi' >/dev/null 2>&1 && : # documents why the gate is a script with an exit status

if [ "$failures" -ne 0 ]; then echo "db-evidence tests: $failures FAILED" >&2; exit 1; fi
echo 'db-evidence tests passed'
