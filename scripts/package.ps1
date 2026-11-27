# Assembles dist/chatty-<version>.zip from a release build:
#   chatty.exe, transcribe.dll + ggml DLLs, llama\ (cleanup pass), licenses.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargo = Get-Content (Join-Path $root 'Cargo.toml') -Raw
if ($cargo -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'version not found' }
$version = $Matches[1]

$release = Join-Path $root 'target\release'
$out = Join-Path $root 'dist\chatty'
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out, (Join-Path $out 'licenses') | Out-Null

Copy-Item (Join-Path $release 'chatty.exe') $out
Copy-Item (Join-Path $root 'native\transcribe\bin\*.dll') $out
Copy-Item -Recurse (Join-Path $root 'native\llama') (Join-Path $out 'llama')
Remove-Item (Join-Path $out 'llama\.version') -ErrorAction SilentlyContinue
Copy-Item -Recurse (Join-Path $root 'native\transcribe\licenses') (Join-Path $out 'licenses\transcribe.cpp')
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $out 'licenses\chatty.txt')
Copy-Item (Join-Path $root 'README.md') $out
Copy-Item (Join-Path $root 'THIRD-PARTY.md') (Join-Path $out 'licenses')

# The prebuilt DLLs carry 1970 timestamps, which zip (pre-1980) rejects.
Get-ChildItem $out -Recurse -File | ForEach-Object { $_.LastWriteTime = Get-Date }

$zip = Join-Path $root "dist\chatty-$version-windows-x64.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path (Join-Path $out '*') -DestinationPath $zip
$exe = (Get-Item (Join-Path $out 'chatty.exe')).Length
Write-Host ("chatty.exe {0:N1} MB, zip {1:N1} MB -> {2}" -f ($exe / 1MB), ((Get-Item $zip).Length / 1MB), $zip)
