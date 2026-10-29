"""Only displays granted by the local portal dialog can be selected remotely."""
def granted_streams(raw):
 if not isinstance(raw,list) or not 1<=len(raw)<=8:raise RuntimeError('Select between one and eight monitors locally')
 result=[];seen=set()
 for node,properties in raw:
  size=properties.get('logical_size',properties.get('size'))
  if type(node)is not int or node<0 or node in seen or not isinstance(size,(list,tuple)) or len(size)!=2 or any(type(v)is not int or not 0<v<=16384 for v in size):raise RuntimeError('Invalid portal monitor geometry')
  seen.add(node);result.append(dict(node=node,width=size[0],height=size[1],properties=properties))
 return result

def status_displays(session):
 result=[]
 for index,v in enumerate(session.streams):
  position=v['properties'].get('position',[0,0]);result.append(dict(id=str(v['node']),name='Shared display '+str(index+1),width=v['width'],height=v['height'],origin_x=position[0],origin_y=position[1],primary=index==0))
 return result

async def select_display(session,display_id):
 async with session.guard:
  await session.ready()
  selected=next((v for v in session.streams if str(v['node'])==str(display_id)),None)
  if selected is None:raise RuntimeError('Display not granted by local portal consent')
  if session.stream and session.stream['node']==selected['node']:return {'selected_display_id':str(selected['node'])}
  session.epoch+=1;session.geometry=None;session.last_frame=None;session.capture_size=None
  await session.media.stop()
  try:
   fd=await session.portal.open_remote(session.session)
   await session.media.start(fd,selected['node'],selected['properties'].get('pipewire-serial'))
   initial=await session.media.snapshot();session.capture_size=(initial[1],initial[2]);session.stream=selected
   return {'selected_display_id':str(selected['node'])}
  except BaseException:await session.invalidate('Selected display capture failed; authorize locally to resume');raise
