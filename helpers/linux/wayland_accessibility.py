"""Read the active user's accessible foreground window without enabling permissions."""
import argparse,asyncio,json,math,os,subprocess,sys
from pathlib import Path
MAX_OUTPUT=1024*1024

def local_permission():
 if sys.platform!='linux' or not os.environ.get('DBUS_SESSION_BUS_ADDRESS'):raise RuntimeError('AT-SPI requires the graphical user session bus')
 enabled=subprocess.run(['gsettings','get','org.gnome.desktop.interface','toolkit-accessibility'],capture_output=True,text=True,timeout=2,check=True).stdout.strip()
 if enabled!='true':raise RuntimeError('Enable GNOME Accessibility locally before inspection')
 state=subprocess.run(['gdbus','call','--session','--dest','org.gnome.ScreenSaver','--object-path','/org/gnome/ScreenSaver','--method','org.gnome.ScreenSaver.GetActive'],capture_output=True,text=True,timeout=2,check=True).stdout.strip()
 if state!='(false,)':raise RuntimeError('Desktop locked or lock state unavailable')

def inspect_tree(bounds=None):
 local_permission()
 import gi
 gi.require_version('Atspi','2.0')
 from gi.repository import Atspi
 if hasattr(Atspi,'set_timeout'):Atspi.set_timeout(400,800)
 desktop=Atspi.get_desktop(0)
 if desktop is None:raise RuntimeError('AT-SPI desktop unavailable')
 active=None
 for app_index in range(min(64,desktop.get_child_count())):
  app=desktop.get_child_at_index(app_index)
  if app is None:continue
  try:owner=os.stat('/proc/'+str(app.get_process_id())).st_uid
  except OSError:continue
  if owner!=os.getuid():continue
  for index in range(min(64,app.get_child_count())):
   window=app.get_child_at_index(index)
   if window and window.get_state_set().contains(Atspi.StateType.ACTIVE):active=window;break
  if active is not None:break
 if active is None:raise RuntimeError('No accessible foreground window belonging to this OS user')
 def rectangle(node):
  component=node.get_component_iface()
  if component is None:return None
  r=component.get_extents(Atspi.CoordType.SCREEN);return dict(x=r.x,y=r.y,width=r.width,height=r.height)
 if bounds:
  r=rectangle(active);x,y,width,height=bounds
  if not r or r['x']+r['width']<=x or r['x']>=x+width or r['y']+r['height']<=y or r['y']>=y+height:raise RuntimeError('Foreground window outside selected display')
 budget=[200]
 def tree(node,depth):
  if depth>=6 or budget[0]<=0:return {'truncated':True}
  budget[0]-=1
  try:
   if not node.get_state_set().contains(Atspi.StateType.SHOWING):return {'offscreen':True}
   protected=node.get_role()==Atspi.Role.PASSWORD_TEXT
   result=dict(role=str(node.get_role_name())[:128],name=''if protected else str(node.get_name()or'')[:512],password=protected)
   r=rectangle(node)
   if r:result['bounds']=r
   if protected:return result
   count=node.get_child_count();children=[]
   for index in range(min(40,count)):
    if budget[0]<=0:break
    child=node.get_child_at_index(index)
    if child:children.append(tree(child,depth+1))
   if children:result['children']=children
   if count>len(children):result['truncated']=True
   return result
  except Exception:return {'unavailable':True}
 result=dict(application_pid=active.get_process_id(),tree=tree(active,0),node_limit=200,truncated=budget[0]<=0,coordinate_space='global-logical',display_bounds=bounds)
 local_permission();return result

async def capture_tree(bounds=None):
 args=[sys.executable,str(Path(__file__).resolve()),'--capture']
 if bounds:args+=['--bounds']+[str(v)for v in bounds]
 child=await asyncio.create_subprocess_exec(*args,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.DEVNULL)
 async def collect():
  output=bytearray()
  while True:
   block=await child.stdout.read(65536)
   if not block:break
   output.extend(block)
   if len(output)>MAX_OUTPUT:raise RuntimeError('AT-SPI output exceeds bound')
  await child.wait();return bytes(output)
 try:
  output=await asyncio.wait_for(collect(),6);result=json.loads(output)
  if child.returncode or 'error'in result:raise RuntimeError(result.get('error','AT-SPI unavailable'))
  return result
 finally:
  if child.returncode is None:child.kill();await child.wait()

if __name__=='__main__':
 p=argparse.ArgumentParser();p.add_argument('--capture',action='store_true');p.add_argument('--bounds',nargs=4,type=float);a=p.parse_args()
 try:
  if a.bounds and(any(not math.isfinite(v)for v in a.bounds)or a.bounds[2]<=0 or a.bounds[3]<=0):raise RuntimeError('Invalid display bounds')
  result=json.dumps(inspect_tree(a.bounds),separators=(',',':')).encode()
  if len(result)>MAX_OUTPUT:raise RuntimeError('AT-SPI output exceeds bound')
  sys.stdout.buffer.write(result)
 except Exception as e:print(json.dumps({'error':str(e)}));sys.exit(1)
