#!/usr/bin/env python3
"""Positive and regression-negative tests for the restore static checker."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
checker = root / "scripts/check_restore_runbook.py"
containerfile = root / "packaging/Containerfile"
deploy = root / "packaging/DEPLOY.md"
script = root / "scripts/rehearse_restore.sh"


def check(text: str, should_pass: bool) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        candidate = Path(tmp) / "rehearse_restore.sh"
        candidate.write_text(text)
        result = subprocess.run(
            ["python3", str(checker), str(containerfile), str(deploy), str(candidate)],
            capture_output=True, text=True,
        )
        assert (result.returncode == 0) == should_pass, result.stdout + result.stderr


original = script.read_text()
check(original, True)
negative_fixtures = {
    "host-side chown": original.replace("TASK=\"track2-rehearsal-", "chown 0:10001 \"$SCRATCH/data\"\nTASK=\"track2-rehearsal-", 1),
    "bad stop/wait order": original.replace('docker stop "$TASK-server" >/dev/null\nset +e; wait "$SERVER_PID";', 'set +e; wait "$SERVER_PID";\ndocker stop "$TASK-server" >/dev/null', 1),
    "implicit image VOLUME": original.replace('"$SCRATCH/data:/var/lib/bread-tracker"', '"$SCRATCH/data:/data"'),
    "inverted invalid assertion": original.replace('docker run --rm --name "$TASK-invalid-check"', 'if docker run --rm --name "$TASK-invalid-check"', 1),
}
for label, fixture in negative_fixtures.items():
    try:
        check(fixture, False)
    except AssertionError as exc:
        raise SystemExit(f"negative fixture not rejected ({label}): {exc}")
print("restore checker positive and 3 negative fixtures passed")
