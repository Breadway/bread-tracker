#!/usr/bin/env python3
"""Exercise invalid-restore assertion polarity without Docker."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
script = (root / "scripts/rehearse_restore.sh").read_text()
outer = script.split('docker run --rm --name "$TASK-invalid-fixture"', 1)[1].split("echo 'stage: invalid candidate rejected", 1)[0]
assert 'if docker run' not in outer, "assertion success is still treated as failure"
assert 'docker run --rm --name "$TASK-invalid-check"' in outer
assert 'before=$(sqlite3 /data/bread-tracker.db' in script
assert 'after=$(sqlite3 /data/bread-tracker.db' in script

with tempfile.TemporaryDirectory() as tmp:
    db = Path(tmp) / "existing.db"
    db.write_text("preserved\n")
    program = '''set -eu
before=$(cat "$DB_FILE")
test "$before" = preserved
if [ "$(printf invalid | grep -q '^ok$' && echo ok || true)" = ok ]; then exit 1; fi
after=$(cat "$DB_FILE")
test "$after" = "$before"
'''
    env = {"PATH": "/usr/bin:/bin", "DB_FILE": str(db)}
    assert subprocess.run(["/bin/sh", "-eu", "-c", program], env=env).returncode == 0
    broken = program.replace('test "$after" = "$before"', 'test "$after" = changed')
    assert subprocess.run(["/bin/sh", "-eu", "-c", broken], env=env).returncode != 0

print("invalid restore assertion control-flow tests passed")
