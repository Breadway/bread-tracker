#!/usr/bin/env python3
"""Static safety checks for the operational restore runbook and rehearsal."""
from pathlib import Path
import re
import subprocess
import sys


def fail(message: str) -> None:
    raise SystemExit(f"restore runbook check failed: {message}")


containerfile = Path(sys.argv[1]).read_text()
deploy = Path(sys.argv[2]).read_text()
script_path = Path(sys.argv[3]) if len(sys.argv) > 3 else Path("scripts/rehearse_restore.sh")
script = script_path.read_text()
match = re.search(r"^ENTRYPOINT\s+\[([^\]]+)\]", containerfile, re.M)
if not match:
    fail("Containerfile ENTRYPOINT is missing or not JSON-array form")
entrypoint = re.findall(r'"([^"]+)"', match.group(1))
if len(entrypoint) != 2 or entrypoint[1] != "/usr/local/bin/bread-tracker-server":
    fail("unexpected image ENTRYPOINT contract")
wrapper = entrypoint[0]
if "mark-restored" not in deploy or f"--entrypoint {wrapper}" not in deploy:
    fail("operational marker must explicitly invoke tracker wrapper")
operational = deploy.split("## Disposable restore", 1)[0]
litestream = re.search(r"docker run --rm --name \"\$TASK-litestream\"[^\n]*(?:\\\n[^\n]*)*", operational)
if not litestream or "--entrypoint" in litestream.group(0):
    fail("operational Litestream restore must use its image default entrypoint")
if "docker compose" not in operational:
    fail("operational service restore instructions missing")
if "scripts/rehearse_restore.sh" not in deploy:
    fail("DEPLOY must invoke the standalone rehearsal script")
rehearsal_docs = deploy.split("## Disposable restore", 1)[-1]
if re.search(r"(?m)^docker\s+", rehearsal_docs):
    fail("DEPLOY must not retain stale inline rehearsal commands")

commands = re.findall(r"(?m)^\s*docker\s+run\b[^\n]*(?:\\\n[^\n]*)*", script)
if not commands:
    fail("rehearsal contains no Docker helpers")
for cmd in commands:
    if "--rm" not in cmd or '--name "$TASK-' not in cmd:
        fail(f"helper must be --rm and task-prefixed: {cmd}")
    if '"$IMG"' in cmd:
        if "/var/lib/bread-tracker" not in cmd:
            fail(f"tracker helper lacks explicit image VOLUME bind: {cmd}")
        if cmd.count(":/var/lib/bread-tracker") != 1:
            fail(f"tracker helper VOLUME destination must be bound exactly once: {cmd}")
        if "--entrypoint" not in cmd or wrapper not in cmd:
            fail(f"tracker helper must explicitly select wrapper entrypoint: {cmd}")
    if "litestream/litestream" in cmd and "--entrypoint" in cmd:
        fail("Litestream helper must use its own default entrypoint")
if re.search(r"(?m)^chown\b", script):
    fail("host-side chown is forbidden; ownership changes belong in root helpers")
if not re.search(r"(?m)^\s*docker run[^\n]*-v \"\$SCRATCH/data:/var/lib/bread-tracker\"", script):
    fail("all tracker image helpers must bind the declared data VOLUME")
if '"$IMG"' not in script:
    fail("no tracker image helper invocations found")
helper_count = sum(1 for line in script.splitlines() if line.startswith('docker run --rm --name "$TASK-'))
if script.count('"$SCRATCH/data:/var/lib/bread-tracker"') < helper_count - 1:
    fail("a tracker helper may rely on an implicit anonymous VOLUME")
if 'docker stop "$TASK-server"' not in script:
    fail("must stop only the exact task server")
if 'name=$(docker inspect --format' not in script or '"/$TASK-server"' not in script:
    fail("must verify exact server container name before stopping")
if 'wait "$SERVER_PID"' not in script:
    fail("must wait on foreground docker run host PID for reliable exit status")
if re.search(r'docker\s+wait\s+"\$TASK-server"', script):
    fail("must not docker-wait a possibly auto-removed container")
server_lifecycle = script.split("# Foreground docker run", 1)[-1]
server_stop = 'docker stop "$TASK-server"'
server_wait = 'wait "$SERVER_PID"'
server_pid_assign = 'SERVER_PID=$!'
if server_stop not in server_lifecycle or server_wait not in server_lifecycle:
    fail("foreground server lifecycle must stop its verified container and wait on its host process")
server_stop_line = server_lifecycle.index(server_stop)
server_wait_line = server_lifecycle.rindex(server_wait)
if not (server_pid_assign in server_lifecycle and server_lifecycle.index(server_pid_assign) < server_stop_line < server_wait_line):
    fail("server must be stopped before waiting on foreground docker run")
verify_line = server_lifecycle.index('name=$(docker inspect --format')
if verify_line > server_stop_line:
    fail("server name must be verified before stop")
if "rm -rf -- \"$SCRATCH\"" not in script or script.index('rm -rf -- "$SCRATCH"') < script.index('docker stop "$TASK-server"'):
    fail("scratch cleanup must follow server stop/wait")
if "chown -R" not in script or "id -u" not in script or "id -g" not in script:
    fail("cleanup must hand root-owned fixtures back to invoking host uid/gid")
if "--user 0" not in script:
    fail("fixture generation must happen inside explicit root helpers")
if '"$SCRATCH/secret:/secret"' not in script or '"$SCRATCH/secret/fcm.json:/run/secrets/fcm.json:ro"' not in script:
    fail("secret must use a distinct host path from recursive data ownership changes")
if "17291" not in script or "ss -ltn" not in script:
    fail("must preflight and publish only the scratch server port")
if "docker compose" in script or "bread-tracker-data:" in script:
    fail("rehearsal must not touch Compose or named production volumes")
for required in ("PRAGMA integrity_check", "PRAGMA wal_checkpoint(TRUNCATE)", "test -f /restore/missing.db", "mark-restored", "healthz", "CHECKSUM"):
    if required not in script:
        fail(f"rehearsal missing required check {required!r}")
if "sha384" not in script or "migration" not in script.lower():
    fail("populated fixture must establish SQLx migration checksum")
if "trap cleanup EXIT" not in script or "trap 'exit 130' INT" not in script or "trap 'exit 143' TERM" not in script:
    fail("cleanup traps missing")
print("restore runbook and rehearsal safety checks passed")
