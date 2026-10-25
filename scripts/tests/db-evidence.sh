#!/usr/bin/env bash
# Tests for scripts/db-evidence.py, the gate behind the upgrade runbook in packaging/DEPLOY.md.
# Builds small real SQLite files, so it needs only python3. Run: scripts/tests/db-evidence.sh
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
tool=$here/db-evidence.py
migrations=$(cd "$here/.." && pwd)/migrations
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() { echo "FAILED: $*" >&2; exit 1; }
passes() { local name=$1; shift; "$@" >"$tmp/out" 2>"$tmp/err" || { cat "$tmp/out" "$tmp/err" >&2; fail "$name should pass"; }; }
stops() {
  local name=$1 want=$2; shift 2
  if "$@" >"$tmp/out" 2>"$tmp/err"; then cat "$tmp/out" >&2; fail "$name should stop"; fi
  grep -q "$want" "$tmp/out" "$tmp/err" || { cat "$tmp/out" "$tmp/err" >&2; fail "$name: expected '$want'"; }
}

# A real upgraded database: the repository's own migrations (1, 2, 3, 5, 6) applied in order,
# recorded the way SQLx records them, with data in it.
python3 - "$tmp/live.db" "$migrations" <<'PY'
import hashlib, sqlite3, sys
db, mig = sys.argv[1:3]
c = sqlite3.connect(db)
c.execute("create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null)")
for v, f in [(1, "0001_init"), (2, "0002_users"), (3, "0003_hygiene_indexes"), (5, "0005_users_forward_repair"), (6, "0006_users_name_key_guard")]:
    sql = open(f"{mig}/{f}.sql", "rb").read()
    c.executescript(sql.decode())
    c.execute("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (?,?,1,?,0)", (v, f, hashlib.sha384(sql).digest()))
c.executescript("""
insert into items (user_id,id,kind,name,points,updated_at,seq) values (1,'i1','chore','Dishes',10,1,1),(1,'i2','habit','Read',5,1,2);
insert into completions (user_id,item_id,date,completed_at,updated_at,seq) values (1,'i1','2026-10-20',1,1,3);
insert into devices (id,user_id,name,token_hash,created_at,last_seen) values ('d1',1,'Pixel','h1',1,1);
update meta set val = 3 where key = 'seq';
""")
c.commit(); c.close()
PY
cp "$tmp/live.db" "$tmp/frozen.db"
cp "$tmp/live.db" "$tmp/replica.db"

# --- check: a good upgraded database passes, and the ledger options are enforced
passes "check good" python3 "$tool" check "$tmp/frozen.db" --ledger-subset 1,2,3,5,6
grep -q "CHECK PASSED" "$tmp/out" || fail "check prints CHECK PASSED"
passes "check exact" python3 "$tool" check "$tmp/frozen.db" --ledger-exact 1,2,3,5,6
stops "ledger too small" "expected" python3 "$tool" check "$tmp/frozen.db" --ledger-exact 1,2,3,6
stops "unsupported version" "not supported by the release" python3 "$tool" check "$tmp/frozen.db" --ledger-subset 1,2,3

# --- a database that already recorded 0004 is refused by the release check
cp "$tmp/frozen.db" "$tmp/with4.db"
python3 - "$tmp/with4.db" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (4,'pair',1,x'00',0)")
c.commit()
PY
stops "recorded 0004" "\[4\] are not supported" python3 "$tool" check "$tmp/with4.db" --ledger-subset 1,2,3,5,6

# --- compare: identical data passes
passes "compare identical" python3 "$tool" compare "$tmp/frozen.db" "$tmp/replica.db"
grep -q "COMPARE PASSED" "$tmp/out" || fail "compare prints COMPARE PASSED"

# --- compare: file times are irrelevant. An old file with the same data passes, and a brand
# new file with different data fails.
touch -d '2001-01-01' "$tmp/replica.db"
passes "old mtime, same data" python3 "$tool" compare "$tmp/frozen.db" "$tmp/replica.db"
cp "$tmp/frozen.db" "$tmp/newer.db"; touch "$tmp/newer.db"
python3 - "$tmp/newer.db" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute("update items set name = 'Washing up' where id = 'i1'"); c.commit()
PY
stops "fresh mtime, different row content" "differs from the frozen copy in data_digest" python3 "$tool" compare "$tmp/frozen.db" "$tmp/newer.db"
grep -q "COMPARE FAILED: do not deploy" "$tmp/out" || fail "compare prints COMPARE FAILED"

# --- compare: stale replica (a write the replica never received)
cp "$tmp/frozen.db" "$tmp/stale-source.db"
python3 - "$tmp/frozen.db" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("insert into items (user_id,id,kind,name,points,updated_at,seq) values (1,'i3','chore','New',1,1,4)")
c.execute("update meta set val = 4 where key = 'seq'")
c.commit()
PY
stops "stale replica: seq" "meta_seq" python3 "$tool" compare "$tmp/frozen.db" "$tmp/stale-source.db"
stops "stale replica: counts" "counts" python3 "$tool" compare "$tmp/frozen.db" "$tmp/stale-source.db"

# --- compare: missing, empty, garbage, corrupt, foreign key violation, wrong ledger
cp "$tmp/live.db" "$tmp/frozen.db"
stops "missing replica" "no such file" python3 "$tool" compare "$tmp/frozen.db" "$tmp/nope.db"
: > "$tmp/empty.db"
stops "empty replica" "empty file" python3 "$tool" compare "$tmp/frozen.db" "$tmp/empty.db"
head -c 4096 /dev/urandom > "$tmp/garbage.db"
stops "garbage replica" "cannot open as a database\|not a bread-tracker\|integrity" python3 "$tool" compare "$tmp/frozen.db" "$tmp/garbage.db"
python3 - "$tmp/live.db" "$tmp/other.db" <<'PY'
import sqlite3, sys
src, dst = sys.argv[1:3]
sqlite3.connect(src).backup(sqlite3.connect(dst))
c = sqlite3.connect(dst)
c.execute("create table notbt (a)")  # not the same schema or data
c.execute("drop table notbt")
c.commit()
PY
python3 - "$tmp/live.db" "$tmp/fk.db" <<'PY'
import sqlite3, sys
src, dst = sys.argv[1:3]
sqlite3.connect(src).backup(sqlite3.connect(dst))
c = sqlite3.connect(dst)
c.execute("pragma foreign_keys = off")
c.execute("insert into items (user_id,id,kind,name,points,updated_at,seq) values (99,'orphan','chore','x',1,1,9)")
c.commit()
PY
stops "foreign key violation" "foreign key violation" python3 "$tool" compare "$tmp/fk.db" "$tmp/fk.db"
python3 - "$tmp/live.db" "$tmp/oldledger.db" <<'PY'
import sqlite3, sys
src, dst = sys.argv[1:3]
sqlite3.connect(src).backup(sqlite3.connect(dst))
c = sqlite3.connect(dst); c.execute("delete from _sqlx_migrations where version = 6"); c.commit()
PY
stops "different ledger" "ledger" python3 "$tool" compare "$tmp/live.db" "$tmp/oldledger.db"
python3 - "$tmp/corrupt.db" "$tmp/live.db" <<'PY'
import shutil, sys
dst, src = sys.argv[1:3]
shutil.copy(src, dst)
b = bytearray(open(dst, "rb").read())
for i in range(4096 * 3, 4096 * 3 + 200):
    b[i] ^= 0xFF
open(dst, "wb").write(bytes(b))
PY
stops "corrupt page" "" python3 "$tool" compare "$tmp/live.db" "$tmp/corrupt.db"

# --- the tool never writes: the frozen copy, with a -wal beside it, is byte-identical after
sha256sum "$tmp/live.db" >"$tmp/before"
python3 "$tool" show "$tmp/live.db" >/dev/null
python3 "$tool" check "$tmp/live.db" >/dev/null
sha256sum -c "$tmp/before" >/dev/null || fail "tool modified the database"

echo 'db-evidence tests passed'
