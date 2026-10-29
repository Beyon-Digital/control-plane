import hashlib,re,subprocess
PATTERN=re.compile(r'^\s*\d+:\s+([+*]*)([^\s]+)\s+(\d+)/(?:\d+)x(\d+)/(?:\d+)([+-]\d+)([+-]\d+)')
def displays():
 text=subprocess.run(['xrandr','--listactivemonitors'],capture_output=True,text=True,timeout=3,check=True).stdout;result=[]
 for line in text.splitlines():
  match=PATTERN.match(line)
  if not match:continue
  flags,name,w,h,x,y=match.groups();w,h,x,y=map(int,(w,h,x,y))
  if len(result)>=16 or not(0<w<=32768 and 0<h<=32768):raise RuntimeError('Invalid monitor geometry/count')
  result.append(dict(id=name,name=name,width=w,height=h,x=x,y=y,origin_x=x,origin_y=y,primary='*'in flags))
 if not result:raise RuntimeError('No active X11 monitors')
 return result

def selected_display(display_id=None):
 choices=displays()
 if display_id is None:return next((v for v in choices if v['primary']),choices[0])
 for v in choices:
  if v['id']==str(display_id):return v
 raise RuntimeError('Selected display unavailable')

def geometry_version(display):
 text='|'.join(str(display[k])for k in('id','x','y','width','height'))
 return int.from_bytes(hashlib.sha256(text.encode()).digest()[:6],'big')
