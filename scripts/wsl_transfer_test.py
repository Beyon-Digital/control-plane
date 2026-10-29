import base64,hashlib,tempfile,unittest,uuid
from pathlib import Path
from wsl_transfer import transfer_request
class BinaryTransfer(unittest.TestCase):
 def setUp(self):self.temp=tempfile.TemporaryDirectory(prefix='cp-binary-');self.root=Path(self.temp.name);self.id=str(uuid.uuid4());self.data=bytes([0,255,128,0,1,2,3])*100;self.sha=hashlib.sha256(self.data).hexdigest()
 def tearDown(self):self.temp.cleanup()
 def call(self,operation,**values):return transfer_request(dict(operation=operation,id=self.id,path='file.bin',**values),self.root)
 def test_binary_roundtrip_create_only_race(self):
  self.call('transfer_begin',size=len(self.data));self.call('transfer_chunk',offset=0,data=base64.b64encode(self.data).decode());(self.root/'file.bin').write_bytes(b'racing writer')
  with self.assertRaises(ValueError):self.call('transfer_commit',sha256=self.sha)
  self.assertEqual((self.root/'file.bin').read_bytes(),b'racing writer');(self.root/'file.bin').unlink();self.call('transfer_commit',sha256=self.sha);meta=self.call('transfer_meta');self.assertEqual(meta['sha256'],self.sha);chunk=self.call('transfer_read',offset=0,length=len(self.data),version=meta['version']);self.assertEqual(base64.b64decode(chunk['data']),self.data)
 def test_replacement_requires_revision_and_checksum(self):
  (self.root/'file.bin').write_bytes(b'old');old=hashlib.sha256(b'old').hexdigest()
  with self.assertRaises(ValueError):self.call('transfer_begin',size=len(self.data))
  self.call('transfer_begin',size=len(self.data),expected_sha256=old);self.call('transfer_chunk',offset=0,data=base64.b64encode(self.data).decode())
  with self.assertRaises(ValueError):self.call('transfer_commit',sha256='0'*64)
  self.call('transfer_commit',sha256=self.sha);self.assertEqual((self.root/'file.bin').read_bytes(),self.data)
 def test_chunk_bounds_abort_and_private_staging(self):
  self.call('transfer_begin',size=2)
  with self.assertRaises(ValueError):self.call('transfer_chunk',offset=0,data=base64.b64encode(b'too long').decode())
  with self.assertRaises(ValueError):transfer_request(dict(operation='transfer_meta',path='.control-plane-transfers/'+self.id+'/data'),self.root)
  self.call('transfer_abort');self.assertFalse((self.root/'.control-plane-transfers'/self.id).exists())
 def test_revision_change_interrupts_download(self):
  (self.root/'file.bin').write_bytes(b'old');meta=self.call('transfer_meta');(self.root/'file.bin').write_bytes(b'changed')
  with self.assertRaises(ValueError):self.call('transfer_read',offset=0,length=3,version=meta['version'])
if __name__=='__main__':unittest.main()
