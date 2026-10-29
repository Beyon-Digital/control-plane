import argparse,subprocess
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('directory',type=Path);p.add_argument('identity');a=p.parse_args()
for path in sorted(a.directory.rglob('*')):
 if path.is_symlink():raise SystemExit('Release payload must not contain symlinks')
 if not path.is_file():continue
 description=subprocess.run(['/usr/bin/file','-b',str(path)],capture_output=True,text=True,check=True,timeout=5).stdout
 if 'Mach-O'in description:
  subprocess.run(['/usr/bin/codesign','--force','--options','runtime','--timestamp','--sign',a.identity,str(path)],check=True);subprocess.run(['/usr/bin/codesign','--verify','--strict',str(path)],check=True)
