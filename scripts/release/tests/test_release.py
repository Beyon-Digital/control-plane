import os,subprocess,sys,tempfile,unittest,xml.etree.ElementTree as ET,base64,json,hashlib,shutil
from pathlib import Path
REPO=Path(__file__).resolve().parents[3]
class ReleaseGates(unittest.TestCase):
 def test_wix_includes_nested_sidecar_and_escapes_publisher(self):
  with tempfile.TemporaryDirectory()as d:
   root=Path(d);payload=root/'input';(payload/'claude/dist').mkdir(parents=True);(payload/'control-plane-host.exe').write_bytes(b'MZ');(payload/'claude/dist/index.mjs').write_text('export {}');(payload/'claude/dist/history.mjs').write_text('export {}');output=root/'release.wxs'
   subprocess.run([sys.executable,str(REPO/'scripts/release/windows_wix.py'),'--input',str(payload),'--output',str(output),'--version','0.1.0','--publisher','A & B'],check=True,capture_output=True)
   tree=ET.parse(output);ns={'w':'http://wixtoolset.org/schemas/v4/wxs'};self.assertEqual(tree.find('w:Package',ns).attrib['Manufacturer'],'A & B');self.assertEqual(len(tree.findall('.//w:File',ns)),3)
 def test_runtime_hash_mismatch_fails_before_execution(self):
  with tempfile.TemporaryDirectory()as d:
   root=Path(d);node=root/'node';node.write_bytes(b'not executable');destination=root/'payload';env=dict(os.environ,NODE_RUNTIME_PATH=str(node),NODE_SHA256='00'*32)
   r=subprocess.run([sys.executable,str(REPO/'scripts/release/stage_runtime.py'),'--platform','linux','--destination',str(destination)],env=env,capture_output=True,text=True);self.assertNotEqual(r.returncode,0);self.assertIn('checksum mismatch',r.stderr);self.assertFalse(destination.exists())
 def test_manifest_requires_pinned_key_before_output(self):
  with tempfile.TemporaryDirectory()as d:
   root=Path(d);artifact=root/'a.deb';artifact.write_bytes(b'abc');out=root/'manifest.json';env=dict(os.environ);env.pop('CONTROL_PLANE_UPDATE_PUBLIC_KEY',None)
   r=subprocess.run([sys.executable,str(REPO/'scripts/release_manifest.py'),'--artifact',str(artifact),'--url','https://updates.example/a.deb','--feed-url','https://updates.example/stable.json','--version','0.2.0','--platform','linux-x86_64','--sequence','2','--signing-key',str(root/'missing.pem'),'--output',str(out)],env=env,capture_output=True,text=True);self.assertNotEqual(r.returncode,0);self.assertFalse(out.exists())
 @unittest.skipUnless(os.name=='posix','POSIX shell gate')
 def test_apple_release_stops_without_signing_credentials(self):
  env={'PATH':os.environ.get('PATH',''),'HOME':os.environ.get('HOME','/tmp')}
  for name in ('macos.sh','ios.sh'):
   r=subprocess.run(['sh',str(REPO/'scripts/release'/name)],cwd=REPO,env=env,capture_output=True,text=True,timeout=5);self.assertNotEqual(r.returncode,0);self.assertIn('required',r.stderr.lower())
 @unittest.skipUnless(shutil.which('openssl'),'OpenSSL fixture signer')
 def test_exact_manifest_signing_round_trip(self):
  with tempfile.TemporaryDirectory()as d:
   root=Path(d);key=root/'fixture.pem';pub=root/'fixture.der';artifact=root/'a.deb';artifact.write_bytes(b'fixture');output=root/'manifest.json'
   subprocess.run(['openssl','genpkey','-algorithm','ED25519','-out',str(key)],check=True,capture_output=True);key.chmod(0o600)
   subprocess.run(['openssl','pkey','-in',str(key),'-pubout','-outform','DER','-out',str(pub)],check=True,capture_output=True)
   env=dict(os.environ,CONTROL_PLANE_UPDATE_PUBLIC_KEY=base64.b64encode(pub.read_bytes()[12:]).decode())
   subprocess.run([sys.executable,str(REPO/'scripts/release_manifest.py'),'--artifact',str(artifact),'--url','https://updates.example/a.deb','--feed-url','https://updates.example/stable.json','--version','0.2.0','--platform','linux-x86_64','--sequence','2','--signing-key',str(key),'--output',str(output)],env=env,check=True,capture_output=True)
   envelope=json.loads(output.read_text());plain=base64.b64decode(envelope['signed']);manifest=json.loads(plain);self.assertEqual(manifest['artifact']['sha256'],hashlib.sha256(b'fixture').hexdigest());(root/'plain').write_bytes(plain);(root/'signature').write_bytes(base64.b64decode(envelope['signature']))
   subprocess.run(['openssl','pkeyutl','-verify','-rawin','-pubin','-keyform','DER','-inkey',str(pub),'-in',str(root/'plain'),'-sigfile',str(root/'signature')],check=True,capture_output=True)
if __name__=='__main__':unittest.main()
