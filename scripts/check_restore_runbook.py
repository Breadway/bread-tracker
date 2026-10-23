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

if "--entrypoint /usr/local/bin/entrypoint-exec" not in deploy:
    fail("runbook does not demonstrate explicit wrapper ENTRYPOINT")
operational = deploy.split("## Disposable restore and existing-volume rehearsal", 1)[0]
for command in re.findall(
    r"(?m)^\s*docker\s+run\b[^\n]*(?:\\\n[^\n]*)*", operational
):
    if "mark-restored" in command and f"--entrypoint {wrapper}" not in command:
        fail("operational mark-restored invocation must explicitly select the wrapper")
    if "--entrypoint" in command and wrapper not in command:
        fail("operational helper invocation must explicitly select the wrapper")
for command in commands:
    if "--entrypoint" in command and wrapper not in command:
        fail("Docker helper invocation overrides ENTRYPOINT with a non-wrapper path")
for required in (
    "test -f /restore/not-present.db",
    "PRAGMA integrity_check",
    "PRAGMA wal_checkpoint(TRUNCATE)",
    "chown 10001:10001",
    "test -r /run/secrets/fcm.json",
    "--name \"$TASK-server\"",
):
    if required not in disposable:
        fail(f"disposable rehearsal is missing {required!r}")
if "bread-tracker-data:" in disposable or "docker compose" in disposable:
    fail("disposable rehearsal must not use Compose services or named volumes")
if "ENTRYPOINT" not in disposable and f"--entrypoint {wrapper}" not in disposable:
    fail("disposable rehearsal does not cover image ENTRYPOINT behavior")
if "--name \"$TASK-server\"" not in disposable or '"$IMG"' not in disposable:
    fail("default ENTRYPOINT foreground server invocation missing")

if "mark-restored" not in operational:
    fail("operational restore instructions missing")
print("restore runbook ENTRYPOINT/argv and disposable safety checks passed")
