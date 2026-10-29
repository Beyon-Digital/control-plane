import argparse,hashlib,re,xml.etree.ElementTree as ET
from pathlib import Path
NS='http://wixtoolset.org/schemas/v4/wxs';ET.register_namespace('',NS)
def node(parent,name,**attrs):return ET.SubElement(parent,'{'+NS+'}'+name,attrs)
def ident(prefix,path):return prefix+hashlib.sha256(path.encode()).hexdigest()[:24]
def main():
 p=argparse.ArgumentParser();p.add_argument('--input',required=True,type=Path);p.add_argument('--output',required=True,type=Path);p.add_argument('--version',required=True);p.add_argument('--publisher',required=True);a=p.parse_args()
 if not re.fullmatch(r'\d+\.\d+\.\d+',a.version)or not a.publisher.strip():raise SystemExit('Version and real publisher required')
 major,minor,patch=map(int,a.version.split('.'))
 if major>255 or minor>255 or patch>65535:raise SystemExit('Version exceeds Windows MSI bounds')
 base=a.input.resolve();root=ET.Element('{'+NS+'}Wix');package=node(root,'Package',Name='Control Plane Host',Manufacturer=a.publisher,Version=a.version,UpgradeCode='D19A157E-7FAD-4AA4-A6BE-41CF91C32F0E',Scope='perMachine')
 node(package,'MajorUpgrade',DowngradeErrorMessage='A newer Control Plane Host is installed.');node(package,'MediaTemplate',EmbedCab='yes');directory=node(node(package,'StandardDirectory',Id='ProgramFiles64Folder'),'Directory',Id='INSTALLFOLDER',Name='Control Plane');feature=node(package,'Feature',Id='Main',Title='Control Plane Host',Level='1');directories={'.':directory};seen=set();count=0
 for path in sorted(base.rglob('*')):
  if path.is_symlink():raise SystemExit('Release payload must not contain symlinks')
  rel=path.relative_to(base);text=rel.as_posix()
  if text.casefold()in seen:raise SystemExit('Case-insensitive payload path collision')
  seen.add(text.casefold())
  if '$('in str(path):raise SystemExit('Payload path contains WiX preprocessor syntax')
  parent=directories[rel.parent.as_posix()]
  if path.is_dir():directories[text]=node(parent,'Directory',Id=ident('D',text),Name=path.name);continue
  if not path.is_file():raise SystemExit('Nonregular payload file')
  count+=1
  if count>50000:raise SystemExit('Excessive release payload file count')
  component_id=ident('C',text);component=node(parent,'Component',Id=component_id,Guid='*',Bitness='always64');node(component,'File',Id=ident('F',text),Source=str(path),KeyPath='yes');node(feature,'ComponentRef',Id=component_id)
 if count==0:raise SystemExit('Empty release payload')
 a.output.parent.mkdir(parents=True,exist_ok=True);ET.indent(root);ET.ElementTree(root).write(a.output,encoding='utf-8',xml_declaration=True);print('Generated WiX payload:',count,'files')
if __name__=='__main__':main()
