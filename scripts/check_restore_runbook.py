#!/usr/bin/env python3
"""Static checks for ENTRYPOINT argv and disposable restore rehearsal docs."""

from pathlib import Path
import re
import sys


def fail(message: str) -> None:
    raise SystemExit(f"restore runbook check failed: {message}")


containerfile = Path(sys.argv[1]).read_text()
deploy = Path(sys.argv[2]).read_text()

match = re.search(r"^ENTRYPOINT\s+\[([^\]]+)\]", containerfile, re.M)
if not match:
    fail("Containerfile ENTRYPOINT is missing or not JSON-array form")
entrypoint = re.findall(r'"([^"]+)"', match.group(1))
if len(entrypoint) != 2 or entrypoint[1] != "/usr/local/bin/bread-tracker-server":
    fail("unexpected image ENTRYPOINT contract")
wrapper = entrypoint[0]

blocks = re.findall(r"```sh\s*\n(.*?)\n```", deploy, re.S)
if not blocks:
    fail("no shell runbook blocks found")
all_commands = "\n".join(blocks)
commands = re.findall(r"(?m)^\s*docker\s+run\b[^\n]*(?:\\\n[^\n]*)*", all_commands)
for command in commands:
    if "--rm" not in command:
        fail(f"docker run is not --rm: {command}")

disposable_start = deploy.index("## Disposable restore and existing-volume rehearsal")
disposable_end = deploy.find("\n## ", disposable_start + 4)
if disposable_end < 0:
    disposable_end = len(deploy)
disposable = deploy[disposable_start:disposable_end]
disposable_commands = re.findall(
    r"(?m)^\s*docker\s+run\b[^\n]*(?:\\\n[^\n]*)*", disposable
)
for command in disposable_commands:
    if "--rm" not in command or '--name "$TASK-' not in command:
        fail(f"disposable container needs --rm and a unique task-prefixed name: {command}")
    if "bread-tracker-server mark-restored" in command and f"--entrypoint {wrapper}" not in command:
        fail("mark-restored invocation must explicitly select the image wrapper entrypoint")
    if "--entrypoint" in command and wrapper not in command:
        fail("utility command overrides must use the image wrapper entrypoint")
    if "litestream/litestream" in command and "--entrypoint" in command:
        fail("Litestream invocation must use its own image entrypoint, not tracker wrapper")
    if '"$IMG"' in command:
        if "/var/lib/bread-tracker" not in command:
            fail(f"tracker helper does not bind image VOLUME /var/lib/bread-tracker: {command}")
        if command.count(":/var/lib/bread-tracker") != 1:
            fail(f"tracker helper must bind the VOLUME destination exactly once: {command}")
        if "--entrypoint" not in command or wrapper not in command:
            if "--name \"$TASK-server\"" not in command:
                fail(f"tracker utility helper must explicitly select the image wrapper: {command}")

if "--entrypoint /usr/local/bin/entrypoint-exec" not in deploy:
    fail("runbook does not demonstrate explicit wrapper ENTRYPOINT")
operational = deploy.split("## Disposable restore and existing-volume rehearsal", 1)[0]
operational_commands = re.findall(
    r"(?m)^\s*docker\s+run\b[^\n]*(?:\\\n[^\n]*)*", operational
)
for command in operational_commands:
    if "mark-restored" in command and f"--entrypoint {wrapper}" not in command:
        fail("operational mark-restored invocation must explicitly select the wrapper")
    if "--entrypoint" in command and wrapper not in command and "litestream/litestream" not in command:
        fail("operational helper invocation must explicitly select the wrapper")
for required in (
    "test -f /restore/not-present.db",
    "PRAGMA integrity_check",
    "PRAGMA wal_checkpoint(TRUNCATE)",
    "chown -R 10001:10001",
    "test -r /run/secrets/fcm.json",
    "--name \"$TASK-server\"",
):
    if required not in disposable:
        fail(f"disposable rehearsal is missing {required!r}")
if "bread-tracker-data:" in disposable or "docker compose" in disposable:
    fail("disposable rehearsal must not use Compose services or named volumes")
if "-p 127.0.0.1:17291:7290" not in disposable or "-p 127.0.0.1:7290:7290" in disposable:
    fail("disposable rehearsal must publish only the non-production host port")
if '"$SCRATCH/data:/data"' not in disposable or 'chown -R 10001:10001 /data' not in disposable:
    fail("full isolated data directory ownership transition is not tested")
if 'chmod 0755 "$SCRATCH"' not in disposable or 'chown 0:10001 "$SCRATCH/data"' not in disposable:
    fail("scratch parent traversal permissions for uid 10001 are not established")
if "trap cleanup EXIT" not in disposable or "trap 'exit 130' INT" not in disposable or "trap 'exit 143' TERM" not in disposable:
    fail("exit, interrupt and termination must run cleanup after the server is stopped")
if 'chown 0:10001 /scratch/dummy-secret' not in disposable:
    fail("dummy secret preparation must run through explicit root helper")
if "--mount \"type=bind,src=$ROOT/migrations/0001_init.sql" not in disposable:
    fail("fixture bind must be narrowly scoped to the migration file")
if "PRAGMA journal_mode=WAL" not in disposable:
    fail("fixture does not exercise a WAL-mode checkpoint")
if 'test "$busy" = 0' not in disposable or 'test "$log_frames" = "$checkpointed"' not in disposable:
    fail("checkpoint must validate busy status and completed frame counts")
if 'test "$log_frames" = "-1"' not in disposable or 'test "$checkpointed" = "-1"' not in disposable:
    fail("checkpoint must handle SQLite non-WAL -1/-1 results")
if 'ss -ltn' not in disposable or '17291' not in disposable:
    fail("disposable server must check/use a non-production host port")
if 'docker wait "$TASK-server"' not in disposable or 'docker stop "$TASK-server"' not in disposable:
    fail("server stop/wait cleanup must be in the same executable task-scoped script")
cleanup = disposable.split("cleanup() {", 1)[1].split("\n}", 1)[0]
if cleanup.find('docker stop "$TASK-server"') < 0 or cleanup.find('docker wait "$TASK-server"') < 0 or cleanup.find('rm -rf "$SCRATCH"') < 0:
    fail("interrupt cleanup must stop the task server before deleting its bind directory")
if cleanup.find('docker wait "$TASK-server"') > cleanup.find('rm -rf "$SCRATCH"'):
    fail("cleanup deletes bound scratch data before stopping server")
if 'grep -qx "0|0|0"' in deploy:
    fail("checkpoint must not assume WAL mode's fixed frame counts")
litestream = re.search(r'docker run --rm --name "\$TASK-litestream"[^\n]*(?:\\\n[^\n]*)*', operational)
if not litestream or '--entrypoint' in litestream.group(0):
    fail("Litestream restore must execute through its own image entrypoint")
if "docker stop \"bread-tracker\"" in disposable or "docker compose" in disposable:
    fail("disposable block must not stop production or use Compose")
if "--mount \"type=bind,src=$ROOT/migrations/0001_init.sql" not in disposable:
    fail("fixture must bind only the needed migration file, not the source checkout")
if "REPLACE_WITH_PRINTED_SUFFIX" in deploy or "another terminal define" in deploy:
    fail("runbook contains a health-stop handoff with undefined task state")
if 'cleanup() {' not in disposable or 'docker stop "$TASK-server"' not in cleanup:
    fail("exit/interrupt trap must stop task server before deleting bind-mounted data")
if "ENTRYPOINT" not in disposable and f"--entrypoint {wrapper}" not in disposable:
    fail("disposable rehearsal does not cover image ENTRYPOINT behavior")
if "--name \"$TASK-server\"" not in disposable or '"$IMG"' not in disposable:
    fail("default ENTRYPOINT foreground server invocation missing")
if 'docker run --rm -d --name "$TASK-server"' not in disposable:
    fail("server probe must run a named, auto-removed disposable instance")
if 'docker wait "$TASK-server"' not in disposable or 'docker stop "$TASK-server"' not in disposable:
    fail("server test must stop and capture exit before scratch cleanup")
if 'chown 0:10001 "$SCRATCH/data" "$SCRATCH/restore"' not in disposable:
    fail("uid 10001 parent-directory access must be set up within scratch only")
if 'cp /work/existing.db /restore/restored.db' not in disposable:
    fail("restored candidate must be prepared in a root helper, not host-chowned")
if '--mount "type=bind,src=$ROOT/migrations/0001_init.sql' not in disposable:
    fail("do not bind-mount the complete source checkout into helper containers")
for command in disposable_commands:
    if '"$IMG"' in command and "/var/lib/bread-tracker" not in command:
        fail(f"tracker helper must bind the declared VOLUME destination: {command}")
    if '"$IMG"' in command and command.count(":/var/lib/bread-tracker") != 1:
        fail(f"tracker helper must bind image VOLUME exactly once: {command}")
own_cmd = re.search(r'docker run --rm --name "\$TASK-own"[^\n]*(?:\\\n[^\n]*)*', operational)
if not own_cmd or 'chown -R 10001:10001 /data' not in operational:
    fail("root-owned volume data directory ownership transition is missing")
own_at = operational.find('chown -R 10001:10001 /data')
marker_at = operational.find('/usr/local/bin/bread-tracker-server mark-restored')
if own_at < 0 or marker_at < 0 or own_at > marker_at:
    fail("operational data directory ownership must happen before marker opens it")
if 'PRAGMA journal_mode=WAL' not in disposable:
    fail("fixture must validate WAL checkpoint behavior")
if 'cp /work/existing.db /restore/restored.db' not in disposable:
    fail("root helper must prepare restored fixture without host chown/read of root-owned file")

if "mark-restored" not in operational:
    fail("operational restore instructions missing")
print("restore runbook ENTRYPOINT/argv and disposable safety checks passed")
