param([string]$DisplayId='')
$ErrorActionPreference='Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
if(-not('ControlPlaneAXSecurity' -as [type])){
Add-Type -TypeDefinition @'
using System;using System.Text;using System.Runtime.InteropServices;using System.Security.Principal;
public static class ControlPlaneAXSecurity {
 [DllImport("user32.dll")]public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")]public static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll")]public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr value);
 [DllImport("user32.dll")]static extern IntPtr OpenInputDesktop(uint flags,bool inherit,uint access);
 [DllImport("user32.dll")]static extern bool CloseDesktop(IntPtr h);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern bool GetUserObjectInformation(IntPtr h,int index,StringBuilder value,uint size,out uint needed);
 [DllImport("kernel32.dll",SetLastError=true)]static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
 [DllImport("advapi32.dll",SetLastError=true)]static extern bool OpenProcessToken(IntPtr process,uint access,out IntPtr token);
 [DllImport("kernel32.dll")]static extern bool CloseHandle(IntPtr h);
 public static bool Interactive(){var d=OpenInputDesktop(0,false,1);if(d==IntPtr.Zero)return false;try{uint n;var text=new StringBuilder(256);return GetUserObjectInformation(d,2,text,512,out n)&&text.ToString()=="Default";}finally{CloseDesktop(d);}}
 public static bool OwnsWindow(IntPtr window){uint pid;GetWindowThreadProcessId(window,out pid);var process=OpenProcess(0x1000,false,pid);if(process==IntPtr.Zero)return false;IntPtr token=IntPtr.Zero;try{if(!OpenProcessToken(process,8,out token))return false;using(var identity=new WindowsIdentity(token)){return identity.User!=null&&identity.User.Equals(WindowsIdentity.GetCurrent().User);}}finally{if(token!=IntPtr.Zero)CloseHandle(token);CloseHandle(process);}}
}
'@
}
function Build-AXTree($Element,[int]$Depth,[ref]$Remaining){
 if($Depth -ge 6 -or $Remaining.Value -le 0){return @{truncated=$true}}
 $Remaining.Value--
 try{
  $current=$Element.Current
  if($current.IsOffscreen){return @{offscreen=$true}}
  $r=$current.BoundingRectangle
  $node=@{role=$current.ControlType.ProgrammaticName;name=$(if($current.IsPassword){''}else{([string]$current.Name).Substring(0,[Math]::Min(512,([string]$current.Name).Length))});enabled=[bool]$current.IsEnabled;password=[bool]$current.IsPassword}
  if(-not $r.IsEmpty){$node.bounds=@{x=($r.X-$screen.Bounds.X);y=($r.Y-$screen.Bounds.Y);width=$r.Width;height=$r.Height}}
  if($current.IsPassword){return $node}
  $children=New-Object System.Collections.Generic.List[object]
  $walker=[System.Windows.Automation.TreeWalker]::ControlViewWalker;$child=$walker.GetFirstChild($Element)
  while($null -ne $child -and $children.Count -lt 40 -and $Remaining.Value -gt 0){$children.Add((Build-AXTree $child ($Depth+1) $Remaining));$child=$walker.GetNextSibling($child)}
  if($children.Count){$node.children=$children.ToArray()};if($null -ne $child){$node.truncated=$true};return $node
 }catch [System.Windows.Automation.ElementNotAvailableException]{return @{unavailable=$true}}
}
try{
 [void][ControlPlaneAXSecurity]::SetThreadDpiAwarenessContext([IntPtr](-4))
 if(-not [ControlPlaneAXSecurity]::Interactive()){throw 'Desktop locked or inaccessible'}
 $window=[ControlPlaneAXSecurity]::GetForegroundWindow()
 if($window -eq [IntPtr]::Zero -or -not [ControlPlaneAXSecurity]::OwnsWindow($window)){throw 'Foreground window unavailable or belongs to another OS identity'}
 $screen=if($DisplayId){[Windows.Forms.Screen]::AllScreens|Where-Object{$_.DeviceName -eq $DisplayId}|Select-Object -First 1}else{[Windows.Forms.Screen]::PrimaryScreen}
 if($null -eq $screen){throw 'Selected display unavailable'}
 $root=[Windows.Automation.AutomationElement]::FromHandle($window);$r=$root.Current.BoundingRectangle;$b=$screen.Bounds
 if($r.IsEmpty -or $r.Right -le $b.Left -or $r.Left -ge $b.Right -or $r.Bottom -le $b.Top -or $r.Top -ge $b.Bottom){throw 'Foreground window outside selected display'}
 $budget=200;$tree=Build-AXTree $root 0 ([ref]$budget)
 if(-not [ControlPlaneAXSecurity]::Interactive()){throw 'Desktop locked during inspection'}
 @{display_id=$screen.DeviceName;coordinate_space='display-local-pixels';application_pid=$root.Current.ProcessId;tree=$tree;node_limit=200;truncated=($budget -le 0)}|ConvertTo-Json -Compress -Depth 24
}catch{@{error=$_.Exception.Message}|ConvertTo-Json -Compress;exit 1}
