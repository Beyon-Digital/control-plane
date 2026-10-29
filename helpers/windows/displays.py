import ctypes,hashlib,sys

def displays():
 if sys.platform!='win32':raise RuntimeError('Windows display enumeration unavailable')
 from ctypes import wintypes
 api=ctypes.WinDLL('user32',use_last_error=True)
 api.SetThreadDpiAwarenessContext.argtypes=[ctypes.c_void_p];api.SetThreadDpiAwarenessContext.restype=ctypes.c_void_p
 api.SetThreadDpiAwarenessContext(ctypes.c_void_p(-4))
 class MONITORINFOEX(ctypes.Structure):
  _fields_=[('cbSize',wintypes.DWORD),('rcMonitor',wintypes.RECT),('rcWork',wintypes.RECT),('dwFlags',wintypes.DWORD),('szDevice',wintypes.WCHAR*32)]
 api.GetMonitorInfoW.argtypes=[wintypes.HANDLE,ctypes.POINTER(MONITORINFOEX)];api.GetMonitorInfoW.restype=wintypes.BOOL
 callback_type=ctypes.WINFUNCTYPE(wintypes.BOOL,wintypes.HANDLE,wintypes.HDC,ctypes.POINTER(wintypes.RECT),wintypes.LPARAM)
 result=[];errors=[]
 def receive(handle,dc,rectangle,param):
  info=MONITORINFOEX();info.cbSize=ctypes.sizeof(info)
  if not api.GetMonitorInfoW(handle,ctypes.byref(info)):errors.append('GetMonitorInfo failed');return False
  r=info.rcMonitor;w=r.right-r.left;h=r.bottom-r.top
  if not(0<w<=32768 and 0<h<=32768)or len(result)>=16:errors.append('Invalid monitor geometry/count');return False
  result.append(dict(id=info.szDevice,name=info.szDevice,x=r.left,y=r.top,origin_x=r.left,origin_y=r.top,width=w,height=h,primary=bool(info.dwFlags&1)));return True
 callback=callback_type(receive)
 api.EnumDisplayMonitors.argtypes=[wintypes.HDC,ctypes.POINTER(wintypes.RECT),callback_type,wintypes.LPARAM];api.EnumDisplayMonitors.restype=wintypes.BOOL
 if not api.EnumDisplayMonitors(None,None,callback,0)or errors or not result:raise RuntimeError('; '.join(errors)or'Display enumeration failed')
 return result

def selected_display(display_id=None):
 choices=displays()
 if display_id is None:return next((item for item in choices if item['primary']),choices[0])
 for item in choices:
  if item['id']==display_id:return item
 raise RuntimeError('Selected display unavailable')

def geometry_version(display):
 text='|'.join(str(display[k])for k in('id','x','y','width','height'))
 return int.from_bytes(hashlib.sha256(text.encode()).digest()[:6],'big')
