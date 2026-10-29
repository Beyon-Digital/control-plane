param([Parameter(Mandatory=$true)][string]$Distribution,[Parameter(Mandatory=$true)][string]$LinuxUser,[Parameter(Mandatory=$true)][string]$Destination,[string]$Source=(Join-Path $PSScriptRoot 'wsl_bridge.py'))
$ErrorActionPreference='Stop'
if($Distribution -notmatch '^[A-Za-z0-9_. -]{1,128}$' -or $Distribution.StartsWith('-')){throw 'Invalid distribution'}
if($LinuxUser -notmatch '^[A-Za-z0-9_-]{1,64}$' -or $LinuxUser.StartsWith('-')){throw 'Invalid Linux user'}
if(-not $Destination.StartsWith('/') -or $Destination.Contains('\') -or $Destination.Contains([char]0) -or $Destination.Split('/') -contains '..'){throw 'Use an absolute Linux destination'}
$Resolved=(Resolve-Path $Source).Path
$Bundle=@{'wsl_bridge.py'=[Convert]::ToBase64String([System.IO.File]::ReadAllBytes($Resolved));'wsl_transfer.py'=[Convert]::ToBase64String([System.IO.File]::ReadAllBytes((Join-Path (Split-Path $Resolved) 'wsl_transfer.py')))}|ConvertTo-Json -Compress
$Payload=[Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($Bundle))
$Installer=@'
import base64,json,os,pathlib,sys,tempfile
destination=pathlib.Path(sys.argv[1])
bundle=json.loads(base64.b64decode(sys.stdin.buffer.read()))
destination.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
for name,encoded in bundle.items():
    assert name in ('wsl_bridge.py','wsl_transfer.py')
    target=destination if name=='wsl_bridge.py' else destination.parent/name
    assert not target.is_symlink(), 'refusing symlink destination'
    data=base64.b64decode(encoded,validate=True)
    fd,temporary=tempfile.mkstemp(prefix='.cp-wsl-',dir=target.parent)
    try:
        with os.fdopen(fd,'wb') as output:output.write(data);output.flush();os.fsync(output.fileno())
        os.chmod(temporary,0o700);os.replace(temporary,target)
    finally:
        if os.path.exists(temporary):os.unlink(temporary)
print('Installed WSL bridge and binary transfer helper')
'@
$Payload | & wsl.exe --distribution $Distribution --user $LinuxUser --exec python3 -c $Installer $Destination
if($LASTEXITCODE -ne 0){throw 'Bridge installation failed; no WSL distribution was installed or changed'}
