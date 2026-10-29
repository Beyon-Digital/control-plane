import argparse,hashlib,os,re,shutil,subprocess,tempfile
from pathlib import Path

def main():
 p=argparse.ArgumentParser();p.add_argument('--platform',required=True,choices=['macos','windows','linux']);p.add_argument('--destination',required=True,type=Path);a=p.parse_args()
 node=Path(os.environ.get('NODE_RUNTIME_PATH',''));expected=os.environ.get('NODE_SHA256','').lower()
 if not re.fullmatch('[0-9a-f]{64}',expected)or not node.is_file()or node.is_symlink():raise SystemExit('Existing NODE_RUNTIME_PATH plus release-approved NODE_SHA256 required')
 digest=hashlib.sha256()
 with node.open('rb')as source:
  for chunk in iter(lambda:source.read(1048576),b''):digest.update(chunk)
 if digest.hexdigest()!=expected:raise SystemExit('Node runtime checksum mismatch')
 a.destination.mkdir(parents=True,exist_ok=True);target=a.destination/('node.exe'if a.platform=='windows'else'node')
 if target.exists():raise SystemExit('Runtime output already exists')
 shutil.copy2(node,target);target.chmod(0o755)
 r=subprocess.run([str(target.resolve()),'--version'],check=True,timeout=10,capture_output=True,text=True);m=re.fullmatch(r'v(\d+)\.\d+\.\d+\s*',r.stdout)
 if not m or int(m.group(1))<22:raise SystemExit('Bundled Node 22+ required')
 repo=Path(__file__).resolve().parents[2];source=repo/'sidecars/claude'
 with tempfile.TemporaryDirectory(prefix='control-plane-sidecar-')as temporary:
  stage=Path(temporary)
  for name in ('package.json','package-lock.json'):shutil.copy2(source/name,stage/name)
  shutil.copytree(source/'src',stage/'src');shutil.copytree(source/'scripts',stage/'scripts');npm='npm.cmd'if os.name=='nt'else'npm'
  subprocess.run([npm,'ci','--ignore-scripts'],cwd=stage,check=True);subprocess.run([npm,'run','build'],cwd=stage,check=True);subprocess.run([npm,'prune','--omit=dev','--ignore-scripts'],cwd=stage,check=True)
  if not(stage/'dist/index.mjs').is_file()or not(stage/'dist/history.mjs').is_file():raise SystemExit('Sidecar build must produce dist/index.mjs and history.mjs')
  output=a.destination/'claude'
  if output.exists():raise SystemExit('Sidecar output already exists')
  output.mkdir()
  for name in ('package.json','package-lock.json'):shutil.copy2(stage/name,output/name)
  shutil.copytree(stage/'dist',output/'dist');shutil.copytree(stage/'node_modules',output/'node_modules',symlinks=False)
 print('Verified runtime and production sidecar prepared:',a.destination)
if __name__=='__main__':main()
