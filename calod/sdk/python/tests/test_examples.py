"""Every adapter example must run end to end and call its stub
provider exactly once, however often the decorated call is retried.

Examples run as subprocesses (the way a user runs them); skipped
when the calod server binary has not been built.
"""

import subprocess
import sys
from pathlib import Path

import pytest

EXAMPLES = Path(__file__).resolve().parents[1] / "examples"
SCRIPTS = [
    "stripe_charge.py",
    "razorpay_upi.py",
    "whatsapp_message.py",
    "email_receipt.py",
]


@pytest.mark.parametrize("script", SCRIPTS)
def test_example_runs_with_exactly_one_provider_call(script):
    path = EXAMPLES / script
    assert path.is_file(), f"missing example {script}"
    proc = subprocess.run(
        [sys.executable, str(path)],
        capture_output=True,
        text=True,
        timeout=240,
    )
    if proc.returncode != 0 and "calod server binary not found" in proc.stderr:
        pytest.skip("calod server binary not built")
    assert proc.returncode == 0, f"{script} failed:\n{proc.stdout}\n{proc.stderr}"
    assert "provider_calls=1" in proc.stdout, proc.stdout
