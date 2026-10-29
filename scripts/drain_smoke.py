#!/usr/bin/env python3
"""Verify a live event socket closes before update maintenance permits replacement."""
import base64,json,os,pathlib,signal,socket,subprocess,tempfile,time,urllib.request,uuid
ROOT=pathlib.Path(__file__).resolve().parents[1];HOST=ROOT/'target/debug/control-plane-host';UPDATER=ROOT/'target/debug/control-plane-update'
def port():
 with socket.socket()as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
def main():
 with tempfile.TemporaryDirectory(prefix='cp-drain-')as temp:
  root=pathlib.Path(temp);data=root/'data';http,rpc=port(),port();command=[str(HOST),'--data',str(data)];secret=subprocess.check_output(command+['enroll'],text=True).strip();serve=command+['serve','--listen',f'127.0.0.1:{http}','--grpc-listen',f'127.0.0.1:{rpc}','--workspace-root',temp];process=subprocess.Popen(serve,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  def request(path,body=None,token=''):
   req=urllib.request.Request(f'http://127.0.0.1:{http}'+path,data=None if body is None else json.dumps(body).encode(),headers={'Content-Type':'application/json','X-Device-Token':token})
   with urllib.request.urlopen(req,timeout=3)as response:return json.load(response)
  try:
   for _ in range(100):
    try:request('/healthz');break
    except OSError:time.sleep(.05)
   token=request('/v1/pair',dict(secret=secret,device_name='Drain fixture'))['token'];session=request('/v1/sessions',dict(provider='codex',workspace=temp,title='Drain fixture',trusted=False,request_id='drain-create'),token)
   stream=socket.create_connection(('127.0.0.1',http),timeout=3);key=base64.b64encode(os.urandom(16)).decode();handshake=f"GET /v1/events?session_id={session['id']} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\nX-Device-Token: {token}\r\n\r\n";stream.sendall(handshake.encode());assert b'101 Switching Protocols' in stream.recv(8192)
   version=subprocess.check_output([str(HOST),'--version'],text=True).strip().split()[-1];marker=data/'update.drain';fd=os.open(marker,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
   with os.fdopen(fd,'w')as file:json.dump(dict(id=str(uuid.uuid4()),created_at=int(time.time()),installed_version=version),file)
   assert process.wait(timeout=15)==0;stream.close();blocked=subprocess.run(serve,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,timeout=5);assert blocked.returncode!=0 and b'maintenance'in blocked.stderr
   subprocess.run([str(UPDATER),'resume','--worker-data',str(data)],check=True,stdout=subprocess.DEVNULL);assert not marker.exists();print('PASS: live event stream drains, worker exits, same-version restart is blocked, explicit recovery works')
  finally:
   if process.poll()is None:process.send_signal(signal.SIGINT)
   try:process.wait(timeout=5)
   except subprocess.TimeoutExpired:process.kill();process.wait()
if __name__=='__main__':main()
