#!/usr/bin/env python3
"""Linux process-ownership tests; no WSL distribution or credentials required."""
import base64
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

BRIDGE = Path(__file__).with_name("wsl_bridge.py")


@unittest.skipUnless(sys.platform == "linux", "Linux proc and PTY ownership test")
class ProcessOwnership(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cp-wsl-test-")
        self.root = Path(self.temp.name)
        self.env = dict(os.environ, HOME=str(self.root))
        self.children = []

    def tearDown(self):
        for child, lease in self.children:
            subprocess.run([sys.executable,str(BRIDGE),"cancel","--lease",lease],env=self.env,capture_output=True)
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=5)
        self.temp.cleanup()

    def start(self, terminal=False):
        import uuid
        lease = str(uuid.uuid4())
        marker = self.root / "descendant.pid"
        # Parent and descendant stay alive without producing output, so the
        # lease watchdog must work independently of output/read activity.
        script = "import subprocess,time,pathlib; p=subprocess.Popen(['sleep','300']); pathlib.Path('descendant.pid').write_text(str(p.pid)); time.sleep(300)"
        config = {"workspace":str(self.root),"roots":[str(self.root)],"argv":[sys.executable,"-c",script],"terminal":terminal}
        child = subprocess.Popen([sys.executable,str(BRIDGE),"run","--lease",lease,"--config",base64.urlsafe_b64encode(json.dumps(config).encode()).decode()],env=self.env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        self.children.append((child,lease))
        deadline = time.monotonic()+5
        while time.monotonic()<deadline and not marker.exists():
            if child.poll() is not None:
                self.fail(child.communicate()[1].decode())
            time.sleep(0.05)
        self.assertTrue(marker.exists())
        return child,lease,int(marker.read_text())

    def assert_gone(self,pid):
        deadline=time.monotonic()+5
        while time.monotonic()<deadline:
            path=Path(f"/proc/{pid}/stat")
            if not path.exists() or path.read_text().rsplit(")",1)[1].split()[0]=="Z":return
            time.sleep(0.05)
        self.fail(f"descendant {pid} survived cancellation")

    def test_cancel_targets_linux_process_group(self):
        child,lease,descendant=self.start()
        result=subprocess.run([sys.executable,str(BRIDGE),"cancel","--lease",lease],env=self.env,capture_output=True)
        self.assertEqual(result.returncode,0,result.stderr)
        child.wait(timeout=5)
        self.assert_gone(descendant)

    def test_expired_lease_reaps_unattended_tree(self):
        child,lease,descendant=self.start()
        path=self.root/".cache/control-plane/wsl-leases"/(lease+".json")
        record=json.loads(path.read_text());record["renewed"]=time.monotonic()-21
        path.write_text(json.dumps(record))
        child.wait(timeout=5)
        self.assert_gone(descendant)

    def test_terminal_lease_owns_tree_and_accepts_resize(self):
        child,lease,descendant=self.start(terminal=True)
        result=subprocess.run([sys.executable,str(BRIDGE),"resize","--lease",lease,"--rows","40","--cols","120"],env=self.env,capture_output=True)
        self.assertEqual(result.returncode,0,result.stderr)
        record=json.loads((self.root/".cache/control-plane/wsl-leases"/(lease+".json")).read_text())
        self.assertEqual((record["rows"],record["cols"]),(40,120))
        child.stdin.close();child.stdin=None
        child.wait(timeout=5)
        self.assert_gone(descendant)


if __name__=="__main__":unittest.main()
