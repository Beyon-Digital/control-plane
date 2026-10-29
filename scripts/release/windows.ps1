$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
Set-Location (Resolve-Path "$PSScriptRoot/../..")
foreach($name in @('WINDOWS_CERT_THUMBPRINT','CONTROL_PLANE_PUBLISHER','CONTROL_PLANE_UPDATE_PUBLIC_KEY','CONTROL_PLANE_UPDATE_FEED','CONTROL_PLANE_UPDATE_SIGNING_KEY','CONTROL_PLANE_ARTIFACT_BASE_URL','BUILD_NUMBER','FFMPEG_PATH','FFMPEG_SHA256','FFMPEG_LICENSE_FILE','NODE_RUNTIME_PATH','NODE_SHA256')){if(-not [Environment]::GetEnvironmentVariable($name)){throw "Required release configuration missing: $name"}}
if($env:WINDOWS_CERT_THUMBPRINT -notmatch '^[A-Fa-f0-9]{40}$' -or $env:BUILD_NUMBER -notmatch '^[0-9]+$'){throw 'Invalid certificate thumbprint or release sequence'}
$cert=Get-Item "Cert:/CurrentUser/My/$env:WINDOWS_CERT_THUMBPRINT"
if(-not $cert.HasPrivateKey -or $cert.NotAfter -le (Get-Date).AddDays(1) -or $cert.Subject -eq $cert.Issuer){throw 'Non-self-signed unexpired distribution certificate required'}
if(-not ($cert.EnhancedKeyUsageList|Where-Object{$_.ObjectId.Value -eq '1.3.6.1.5.5.7.3.3'})){throw 'Certificate must permit code signing'}
Get-Command cargo,wix,signtool.exe,python|Out-Null
if((Get-FileHash $env:FFMPEG_PATH -Algorithm SHA256).Hash -ne $env:FFMPEG_SHA256){throw 'FFmpeg differs from approved SHA-256'}
function Run-Checked([string]$Tool,[string[]]$Arguments){& $Tool @Arguments;if($LASTEXITCODE -ne 0){throw "${Tool} failed: $LASTEXITCODE"}}
$version=(& python -c 'import tomllib;print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])').Trim()
Run-Checked cargo @('test','--workspace','--locked')
Run-Checked cargo @('build','--release','--locked')
$inputDir=Join-Path (Get-Location) 'build/release-windows/input'
if(Test-Path $inputDir){throw 'Refusing to overwrite release staging'}
New-Item -ItemType Directory -Path $inputDir -Force|Out-Null
foreach($name in @('control-plane-host','control-plane-broker','control-plane-computer-tools','control-plane-update')){Copy-Item "target/release/$name.exe" "$inputDir/$name.exe"}
Copy-Item helpers/windows/*.ps1 $inputDir
Run-Checked python @('scripts/release/stage_runtime.py','--platform','windows','--destination',$inputDir)
Run-Checked python @('-m','PyInstaller','--noconfirm','--onefile','--name','control-plane-video','--distpath',$inputDir,'--workpath','build/release-windows/pyinstaller','--specpath','build/release-windows','helpers/windows/VideoHelper.py')
Copy-Item $env:FFMPEG_PATH "$inputDir/ffmpeg.exe"
Copy-Item $env:FFMPEG_LICENSE_FILE "$inputDir/FFMPEG-LICENSE.txt"
Get-ChildItem $inputDir -Filter '*.ps1'|ForEach-Object{$s=Set-AuthenticodeSignature -FilePath $_.FullName -Certificate $cert -HashAlgorithm SHA256 -TimestampServer 'http://timestamp.digicert.com';if($s.Status -ne 'Valid'){throw "Script signature invalid: $($_.Name)"}}
Get-ChildItem $inputDir -Recurse -File|Where-Object{$_.Extension -in @('.exe','.dll','.node')}|ForEach-Object{
 $stream=[IO.File]::OpenRead($_.FullName);try{$first=$stream.ReadByte();$second=$stream.ReadByte()}finally{$stream.Dispose()}
 if($first -eq 77 -and $second -eq 90){Run-Checked signtool.exe @('sign','/sha1',$env:WINDOWS_CERT_THUMBPRINT,'/fd','SHA256','/tr','http://timestamp.digicert.com','/td','SHA256',$_.FullName);Run-Checked signtool.exe @('verify','/pa','/all',$_.FullName)}
}
New-Item -ItemType Directory -Path dist -Force|Out-Null
$package=Join-Path (Get-Location) "dist/ControlPlane-$version-windows-x86_64.msi"
if(Test-Path $package){throw 'Installer already exists'}
Run-Checked python @('scripts/release/windows_wix.py','--input',$inputDir,'--output','build/release-windows/windows.wxs','--version',$version,'--publisher',$env:CONTROL_PLANE_PUBLISHER)
Run-Checked wix @('build','build/release-windows/windows.wxs','-arch','x64','-o',$package)
Run-Checked signtool.exe @('sign','/sha1',$env:WINDOWS_CERT_THUMBPRINT,'/fd','SHA256','/tr','http://timestamp.digicert.com','/td','SHA256',$package)
Run-Checked signtool.exe @('verify','/pa','/all',$package)
$url=$env:CONTROL_PLANE_ARTIFACT_BASE_URL.TrimEnd('/')+'/'+[IO.Path]::GetFileName($package)
Run-Checked python @('scripts/release_manifest.py','--artifact',$package,'--url',$url,'--feed-url',$env:CONTROL_PLANE_UPDATE_FEED,'--version',$version,'--platform','windows-x86_64','--sequence',$env:BUILD_NUMBER,'--signing-key',$env:CONTROL_PLANE_UPDATE_SIGNING_KEY,'--output',"dist/stable-windows-x86_64-$env:BUILD_NUMBER.json")
Write-Output 'Signed MSI and publisher manifest prepared; nothing published.'
