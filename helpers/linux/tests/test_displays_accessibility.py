import asyncio,importlib.util,os,sys,unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from wayland_displays import granted_streams,status_displays,select_display
from x11_displays import selected_display,geometry_version
from wayland_accessibility import capture_tree,local_permission
class Displays(unittest.TestCase):
 def test_grants_bounded_and_distinct(self):
  values=granted_streams([[1,{'size':[1920,1080]}],[2,{'logical_size':[1280,720],'position':[-1280,0]}]]);self.assertEqual(values[1]['width'],1280)
  for bad in ([],[[1,{'size':[0,10]}]],[[1,{'size':[10,10]}],[1,{'size':[10,10]}]]):
   with self.assertRaises(RuntimeError):granted_streams(bad)
 def test_catalog_stable_ids_and_origins(self):
  s=SimpleNamespace(streams=granted_streams([[17,{'size':[1920,1080],'position':[-1920,0]}]]));self.assertEqual(status_displays(s)[0]['id'],'17');self.assertEqual(status_displays(s)[0]['origin_x'],-1920)
 def test_missing_x11_id_never_falls_back(self):
  item=dict(id='DP-1',name='DP-1',width=1920,height=1080,x=0,y=0,primary=True)
  with patch('x11_displays.displays',return_value=[item]):
   self.assertEqual(selected_display()['id'],'DP-1')
   with self.assertRaises(RuntimeError):selected_display('HDMI-1')
  self.assertNotEqual(geometry_version(item),geometry_version(dict(item,x=1920)))
 def test_windows_geometry_parity(self):
  path=Path(__file__).resolve().parents[2]/'windows'/'displays.py';spec=importlib.util.spec_from_file_location('win_displays',path);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);item=dict(id='DISPLAY1',x=-1920,y=0,width=1920,height=1080);self.assertEqual(module.geometry_version(item),geometry_version(item))
 def test_accessibility_disabled_fails_before_inspection(self):
  with patch('wayland_accessibility.sys.platform','linux'),patch.dict(os.environ,{'DBUS_SESSION_BUS_ADDRESS':'unix:path=/fake'}),patch('wayland_accessibility.subprocess.run',return_value=SimpleNamespace(stdout='false')):
   with self.assertRaisesRegex(RuntimeError,'Enable GNOME Accessibility'):local_permission()
class Cancellation(unittest.IsolatedAsyncioTestCase):
 async def test_cancel_kills_and_reaps_inspection(self):
  class Child:
   returncode=None
   def __init__(self):self.killed=False;self.entered=asyncio.Event();self.stdout=self
   async def read(self,count):self.entered.set();await asyncio.sleep(60)
   def kill(self):self.killed=True
   async def wait(self):self.returncode=-9;return -9
  child=Child()
  async def spawn(*a,**k):return child
  with patch('wayland_accessibility.asyncio.create_subprocess_exec',side_effect=spawn):
   task=asyncio.create_task(capture_tree());await child.entered.wait();task.cancel()
   with self.assertRaises(asyncio.CancelledError):await task
  self.assertTrue(child.killed)
if __name__=='__main__':unittest.main()
