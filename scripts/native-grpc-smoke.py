#!/usr/bin/env python3
"""Real native HTTP/2 acceptance against a disposable host, without inference."""
import os
import subprocess
import sys
from host_fixture import ROOT, native_host

if __name__ == "__main__":
    with native_host() as fixture:
        subprocess.run(["swift", "test", "--package-path", str(ROOT / "apple")]
            + ([] if "--all-tests" in sys.argv else ["--filter", "NativeGRPCAcceptanceTests"]),
            env=dict(os.environ, **fixture), cwd=ROOT, check=True, timeout=300)
