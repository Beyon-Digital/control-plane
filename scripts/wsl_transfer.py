"""Bounded binary transfers, executed under the configured Linux user and roots."""
import base64,hashlib,json,os,shutil,stat,uuid
from pathlib import PurePosixPath
CHUNK=1024*1024
MAX=1024*1024*1024

def relative(root,value):
 if not isinstance(value,str) or '\\' in value or '\0' in value:raise ValueError('invalid relative file path')
 value=PurePosixPath(value)
 if value.is_absolute() or '..' in value.parts or '.control-plane-transfers' in value.parts:raise ValueError('file path escapes workspace or targets internal staging')
 path=root
 for component in value.parts:
  path=path/component
  if path.is_symlink():raise ValueError('symlinks are not exposed by binary transfer')
 return path

def version(info):return f'{info.st_dev}:{info.st_ino}:{info.st_size}:{info.st_mtime_ns}'

def metadata(path,maximum):
 try:info=path.stat()
 except FileNotFoundError:return {'exists':False}
 if not stat.S_ISREG(info.st_mode) or info.st_size>maximum:raise ValueError('binary file exceeds transfer limit')
 digest=hashlib.sha256();count=0
 with path.open('rb') as source:
  while True:
   data=source.read(65536)
   if not data:break
   count+=len(data)
   if count>maximum:raise ValueError('binary file exceeds transfer limit')
   digest.update(data)
  after=os.fstat(source.fileno())
 if version(info)!=version(after) or count!=info.st_size:raise ValueError('conflict: binary file changed')
 return {'exists':True,'size':count,'sha256':digest.hexdigest(),'version':version(info)}

def slot(config,root,create=False):
 identifier=config['id']
 if str(uuid.UUID(identifier))!=identifier:raise ValueError('invalid binary transfer identifier')
 destination=relative(root,config['path']);directory=destination.parent/'.control-plane-transfers'
 if create:directory.mkdir(mode=0o700,exist_ok=True)
 if directory.is_symlink():raise ValueError('invalid staging directory')
 if not directory.exists():raise FileNotFoundError('staging directory absent')
 if not directory.is_dir():raise ValueError('invalid staging directory')
 info=directory.stat()
 if info.st_uid!=os.getuid() or stat.S_IMODE(info.st_mode)&0o077:raise ValueError('staging directory must be user-owned and private')
 work=directory/identifier
 if work.is_symlink():raise ValueError('invalid binary staging path')
 if create:work.mkdir(mode=0o700)
 return destination,work

def load(config,root):
 destination,work=slot(config,root)
 with (work/'metadata.json').open() as source:record=json.load(source)
 if record['workspace']!=str(root) or record['path']!=config['path']:raise ValueError('binary transfer workspace mismatch')
 return destination,work,record

def transfer_request(config,root):
 operation=config['operation'];path=relative(root,config.get('path',''));maximum=int(config.get('max_bytes',MAX))
 if maximum<0 or maximum>MAX:raise ValueError('invalid binary file limit')
 if operation=='transfer_meta':return metadata(path,maximum)
 if operation=='transfer_read':
  info=path.stat()
  if version(info)!=config['version']:raise ValueError('conflict: file changed during download')
  offset=int(config['offset']);length=int(config['length'])
  if offset<0 or length<0 or length>CHUNK or offset+length>info.st_size:raise ValueError('invalid binary chunk bounds')
  with path.open('rb') as source:source.seek(offset);data=source.read(length);after=os.fstat(source.fileno())
  if len(data)!=length or version(after)!=config['version']:raise ValueError('conflict: file changed during download')
  return {'data':base64.b64encode(data).decode('ascii')}
 if operation=='transfer_begin':
  size=int(config['size'])
  if size<0 or size>MAX:raise ValueError('invalid upload size')
  expected=config.get('expected_sha256');actual=metadata(path,MAX)
  if actual['exists']:
   if expected!=actual['sha256']:raise ValueError('conflict: existing file requires current revision')
  elif expected is not None:raise ValueError('conflict: expected destination no longer exists')
  destination,work=slot(config,root,create=True)
  try:
   with (work/'metadata.json').open('x') as output:json.dump({'workspace':str(root),'path':config['path'],'size':size,'expected':expected},output)
   descriptor=os.open(work/'data',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600);os.close(descriptor)
  except BaseException:shutil.rmtree(work,ignore_errors=True);raise
  return {'accepted':True}
 if operation=='transfer_abort':
  try:destination,work=slot(config,root)
  except FileNotFoundError:return {'aborted':True}
  if work.exists():shutil.rmtree(work)
  return {'aborted':True}
 destination,work,record=load(config,root)
 if operation=='transfer_chunk':
  data=base64.b64decode(config['data'],validate=True);offset=int(config['offset'])
  if len(data)>CHUNK or (work/'data').stat().st_size!=offset or offset+len(data)>record['size']:raise ValueError('binary upload chunk violates reserved size or offset')
  with (work/'data').open('ab') as output:output.write(data)
  return {'received':offset+len(data)}
 if operation=='transfer_commit':
  staged=work/'data';info=metadata(staged,MAX)
  if info['size']!=record['size'] or info['sha256']!=config['sha256']:raise ValueError('upload checksum or size mismatch')
  actual=metadata(destination,MAX)
  if actual['exists']:
   if record['expected']!=actual['sha256']:raise ValueError('conflict: destination changed during upload')
   os.chmod(staged,stat.S_IMODE(destination.stat().st_mode)&0o777)
  elif record['expected'] is not None:raise ValueError('conflict: destination was removed during upload')
  with staged.open('rb') as source:os.fsync(source.fileno())
  if record['expected'] is None:
   try:os.link(staged,destination)
   except FileExistsError:raise ValueError('conflict: destination appeared before commit') from None
   staged.unlink()
  else:os.replace(staged,destination)
  shutil.rmtree(work);return {'sha256':info['sha256'],'size':info['size']}
 raise ValueError('unsupported binary transfer operation')
