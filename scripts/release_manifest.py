#!/usr/bin/env python3
"""Sign an exact update manifest with an existing Ed25519 key. Does not publish."""
import argparse,base64,hashlib,json,os,re,subprocess,tempfile,time
from pathlib import Path
from urllib.parse import urlsplit

def main():
 p=argparse.ArgumentParser();p.add_argument('--artifact',required=True,type=Path);p.add_argument('--url',required=True);p.add_argument('--feed-url',required=True);p.add_argument('--version',required=True);p.add_argument('--platform',required=True,choices=['macos-arm64','windows-x86_64','linux-x86_64']);p.add_argument('--sequence',required=True,type=int);p.add_argument('--expires-days',type=int,default=30);p.add_argument('--signing-key',required=True,type=Path);p.add_argument('--output',required=True,type=Path);a=p.parse_args()
 expected=os.environ.get('CONTROL_PLANE_UPDATE_PUBLIC_KEY')
 if not expected:raise SystemExit('Pinned CONTROL_PLANE_UPDATE_PUBLIC_KEY required')
 expected=base64.b64decode(expected,validate=True)
 if len(expected)!=32:raise SystemExit('Ed25519 public key must contain 32 bytes')
 if not re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)',a.version):raise SystemExit('Stable major.minor.patch required')
 if not 1<=a.sequence<2**64 or not 1<=a.expires_days<=31:raise SystemExit('Invalid sequence/expiry')
 def origin(text):
  u=urlsplit(text)
  if u.scheme!='https' or not u.hostname or u.username or u.password or u.fragment:raise SystemExit('HTTPS URL without credentials/fragment required')
  return(u.scheme,u.hostname.lower(),u.port or 443)
 if origin(a.url)!=origin(a.feed_url):raise SystemExit('Artifact/feed origins differ')
 if not a.artifact.is_file()or a.artifact.is_symlink():raise SystemExit('Regular artifact required')
 if not a.signing_key.is_file()or a.signing_key.is_symlink():raise SystemExit('Existing private key required')
 if os.name=='posix' and(a.signing_key.stat().st_uid!=os.getuid()or a.signing_key.stat().st_mode&0o077):raise SystemExit('Private signing key must be owner-only')
 size=a.artifact.stat().st_size
 if not 0<size<=1024**3:raise SystemExit('Artifact outside size bounds')
 digest=hashlib.sha256()
 with a.artifact.open('rb')as source:
  for chunk in iter(lambda:source.read(1024*1024),b''):digest.update(chunk)
 fmt={'macos-arm64':'pkg','windows-x86_64':'msi','linux-x86_64':'deb'}[a.platform]
 if a.artifact.suffix.lower()!='.'+fmt:raise SystemExit('Artifact suffix/platform mismatch')
 stamp=int(time.time());m=dict(schema=1,channel='stable',platform=a.platform,version=a.version,sequence=a.sequence,published_at=stamp,expires_at=stamp+a.expires_days*86400,artifact=dict(url=a.url,size=size,sha256=digest.hexdigest(),format=fmt));payload=json.dumps(m,sort_keys=True,separators=(',',':')).encode()
 with tempfile.TemporaryDirectory(prefix='control-plane-sign-')as directory:
  d=Path(directory);plain=d/'payload';signature=d/'signature';public=d/'public.pem';der=d/'public.der';plain.write_bytes(payload)
  passin=['-passin','env:CONTROL_PLANE_UPDATE_KEY_PASSWORD']if os.environ.get('CONTROL_PLANE_UPDATE_KEY_PASSWORD')else[]
  subprocess.run(['openssl','pkey','-in',str(a.signing_key),*passin,'-pubout','-out',str(public)],check=True)
  subprocess.run(['openssl','pkey','-pubin','-in',str(public),'-outform','DER','-out',str(der)],check=True)
  key=der.read_bytes()
  if key[:12]!=bytes.fromhex('302a300506032b6570032100')or key[12:]!=expected:raise SystemExit('Private key differs from pinned publisher key')
  subprocess.run(['openssl','pkeyutl','-sign','-rawin','-inkey',str(a.signing_key),*passin,'-in',str(plain),'-out',str(signature)],check=True);sig=signature.read_bytes()
  if len(sig)!=64:raise SystemExit('Invalid Ed25519 signature')
  subprocess.run(['openssl','pkeyutl','-verify','-rawin','-pubin','-inkey',str(public),'-in',str(plain),'-sigfile',str(signature)],check=True)
  envelope=dict(signed=base64.b64encode(payload).decode(),signature=base64.b64encode(sig).decode());a.output.parent.mkdir(parents=True,exist_ok=True)
  with a.output.open('x')as output:json.dump(envelope,output,separators=(',',':'));output.write('\n')
 print('Signed manifest prepared:',a.output);print('Artifact SHA-256:',digest.hexdigest())
if __name__=='__main__':main()
