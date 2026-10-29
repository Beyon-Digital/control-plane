# Runs in the signed-in user's interactive session. Never install as a service.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class DesktopInput {
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public UNION data; }
  [StructLayout(LayoutKind.Explicit)] public struct UNION { [FieldOffset(0)] public MOUSE mouse; [FieldOffset(0)] public KEY key; }
  [StructLayout(LayoutKind.Sequential)] public struct MOUSE { public int x,y; public uint wheel,flags,time; public UIntPtr extra; }
  [StructLayout(LayoutKind.Sequential)] public struct KEY { public ushort vk,scan; public uint flags,time; public UIntPtr extra; }
  [DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint count, INPUT[] data, int size);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x,int y);
  [DllImport("user32.dll")] static extern IntPtr OpenInputDesktop(uint flags,bool inherit,uint access);
  [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern bool GetUserObjectInformation(IntPtr obj,int index,StringBuilder name,uint length,out uint required);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr value);
  public static bool Interactive() {
    var desktop=OpenInputDesktop(0,false,1); if(desktop==IntPtr.Zero)return false;
    try { uint needed; var name=new StringBuilder(256); return GetUserObjectInformation(desktop,2,name,512,out needed) && name.ToString()=="Default"; }
    finally { CloseDesktop(desktop); }
  }
  public static void Mouse(uint flags,int amount=0) { var i=new INPUT {type=0,data=new UNION {mouse=new MOUSE {flags=flags,wheel=unchecked((uint)amount)}}}; Emit(i); }
  public static void Key(ushort code,uint flags,ushort scan=0) { var i=new INPUT {type=1,data=new UNION {key=new KEY {vk=code,flags=flags,scan=scan}}}; Emit(i); }
  static void Emit(INPUT i) { if(SendInput(1,new[]{i},Marshal.SizeOf(typeof(INPUT)))!=1) throw new Exception("Input blocked by Windows permissions or integrity boundary"); }
}
'@
function Geometry($r) {$text="$($selected.DeviceName)|$($r.X)|$($r.Y)|$($r.Width)|$($r.Height)";$sha=[Security.Cryptography.SHA256]::Create();try{$b=$sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($text));$h=[BitConverter]::ToString($b).Replace('-','');return [Convert]::ToInt64($h.Substring(0,12),16)}finally{$sha.Dispose()}}
try {
    $raw=[Console]::In.ReadToEnd()
    if ($raw.Length -gt 65536) { throw 'Request exceeds limit' }
    $request=$raw | ConvertFrom-Json
    [void][DesktopInput]::SetThreadDpiAwarenessContext([IntPtr](-4))
    $selected=if($request.display_id){[Windows.Forms.Screen]::AllScreens|Where-Object{$_.DeviceName -eq [string]$request.display_id}|Select-Object -First 1}else{[Windows.Forms.Screen]::PrimaryScreen}
    if($null -eq $selected){throw 'Selected display unavailable'}
    $screen=$selected.Bounds
    $interactive=[DesktopInput]::Interactive()
    if ($request.command -eq 'status') {
        @{available=$interactive;platform='windows';capture_available=$interactive;input_available=$interactive;reason=$(if($interactive){'Ready'}else{'No accessible unlocked interactive desktop'});accessibility=$true;selected_display_id=$selected.DeviceName;displays=@([Windows.Forms.Screen]::AllScreens|ForEach-Object{@{id=$_.DeviceName;name=$_.DeviceName;width=$_.Bounds.Width;height=$_.Bounds.Height;origin_x=$_.Bounds.X;origin_y=$_.Bounds.Y;primary=$_.Primary}})} | ConvertTo-Json -Compress -Depth 5
        exit 0
    }
    if (!$interactive) { throw 'Desktop unavailable or locked' }
    if ($request.command -eq 'select_display') {@{selected_display_id=$selected.DeviceName}|ConvertTo-Json -Compress;exit 0}
    if ($request.command -eq 'accessibility') {& "$PSScriptRoot/AccessibilityHelper.ps1" -DisplayId $selected.DeviceName;exit $LASTEXITCODE}
    if ($request.command -eq 'frame') {
        $image=New-Object System.Drawing.Bitmap($screen.Width,$screen.Height)
        try {
            $graphics=[System.Drawing.Graphics]::FromImage($image)
            try { $graphics.CopyFromScreen($screen.Location,[System.Drawing.Point]::Empty,$screen.Size) } finally {$graphics.Dispose()}
            $stream=New-Object System.IO.MemoryStream
            try { $image.Save($stream,[System.Drawing.Imaging.ImageFormat]::Jpeg);$encoded=[Convert]::ToBase64String($stream.ToArray()) } finally {$stream.Dispose()}
        } finally {$image.Dispose()}
        @{display_id=$selected.DeviceName;image_base64=$encoded;mime_type='image/jpeg';width=$screen.Width;height=$screen.Height;geometry_version=(Geometry $screen)} | ConvertTo-Json -Compress
    } elseif ($request.command -eq 'action') {
        if ($request.geometry_version -ne (Geometry $screen)) { throw 'Desktop geometry changed; capture a new frame' }
        $a=$request.action
        switch ($a.type) {
            'click' {
                $x=[double]$a.x;$y=[double]$a.y
                if([double]::IsNaN($x) -or [double]::IsNaN($y) -or $x -lt 0 -or $y -lt 0 -or $x -ge $screen.Width -or $y -ge $screen.Height){throw 'Click outside captured display'}
                if(![DesktopInput]::SetCursorPos($screen.X+[int]$x,$screen.Y+[int]$y)){throw 'Cannot move pointer'}
                if($a.button -eq 'right'){[DesktopInput]::Mouse(8);[DesktopInput]::Mouse(16)}else{[DesktopInput]::Mouse(2);[DesktopInput]::Mouse(4)}
            }
            'text' {
                $text=[string]$a.text;if([Text.Encoding]::UTF8.GetByteCount($text) -gt 4096){throw 'Text exceeds limit'}
                foreach($c in $text.ToCharArray()){[DesktopInput]::Key(0,4,[ushort]$c);[DesktopInput]::Key(0,6,[ushort]$c)}
            }
            'key' {
                $keys=@{enter=13;return=13;tab=9;space=32;backspace=8;escape=27;left=37;up=38;right=39;down=40;delete=46;home=36;end=35;pageup=33;pagedown=34;a=65;c=67;v=86;x=88;z=90;s=83}
                $code=$keys[[string]$a.key];if(!$code){throw 'Unsupported key'}
                $pressed=@();try {
                    foreach($modifier in $a.modifiers){$m=@{control=17;shift=16;option=18;command=91}[[string]$modifier];if(!$m){throw 'Unsupported modifier'};[DesktopInput]::Key($m,0);$pressed+=,$m}
                    [DesktopInput]::Key($code,0);[DesktopInput]::Key($code,2)
                }finally{[array]::Reverse($pressed);foreach($m in $pressed){[DesktopInput]::Key($m,2)}}
            }
            'scroll' {
                $dx=if($null -ne $a.dx){[int]$a.dx}else{[int]$a.delta_x};$dy=if($null -ne $a.dy){[int]$a.dy}else{[int]$a.delta_y}
                if([Math]::Abs($dx) -gt 1200 -or [Math]::Abs($dy) -gt 1200){throw 'Invalid scroll distance'}
                if($dy){[DesktopInput]::Mouse(2048,$dy)};if($dx){[DesktopInput]::Mouse(4096,$dx)}
            }
            default {throw 'Unsupported action'}
        }
        @{success=$true} | ConvertTo-Json -Compress
    } else {throw 'Unsupported command'}
} catch {
    @{error=$_.Exception.Message} | ConvertTo-Json -Compress
    exit 1
}
