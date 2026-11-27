# Downloads the prebuilt native libraries chatty links against or loads at
# runtime. Nothing here is compiled.
#
#   native/transcribe/   install prefix for transcribe-cpp-sys (TRANSCRIBE_DIR)
#     bin/*.dll          transcribe.dll + ggml DLLs (copied next to chatty.exe)
#     lib/transcribe.lib import library generated from transcribe.dll's exports
#     lib/transcribe-link.json  link manifest the sys crate's build.rs reads
#   native/llama/        llama.cpp Vulkan DLLs for the cleanup pass
#
# The transcribe.cpp version is read from Cargo.toml (the transcribe-cpp
# dependency), so it is pinned in exactly one place.
[CmdletBinding()]
param([switch]$Force)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = Split-Path -Parent $PSScriptRoot
$native = Join-Path $root 'native'

# --- pins -------------------------------------------------------------------
$cargo = Get-Content (Join-Path $root 'Cargo.toml') -Raw
if ($cargo -notmatch 'transcribe-cpp\s*=\s*\{\s*version\s*=\s*"=?([0-9.]+)"') {
    throw 'transcribe-cpp version not found in Cargo.toml'
}
$tcVersion = $Matches[1]
$tcSha256 = @{
    '0.5.1' = '166c38a7f65bed1ee2d6e7b688e7e99da3092469e185ecb1edd618231164bf86'
}[$tcVersion]
$llamaBuild = 'b12774'
$llamaSha256 = 'd29a16265d26ef7598209caaf1a544cbfa6abe2858c24b0ce4ae6bff87a76f22'

function Get-Verified([string]$url, [string]$out, [string]$sha) {
    if (-not (Test-Path $out)) {
        Write-Host "download $url"
        & curl.exe -fsSL --retry 3 -o $out $url
        if ($LASTEXITCODE -ne 0) { throw "download failed: $url" }
    }
    if ($sha) {
        $actual = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
        if ($actual -ne $sha) { Remove-Item $out; throw "sha256 mismatch for $out ($actual)" }
    }
}

function Find-MsvcTool([string]$name) {
    $cmd = Get-Command $name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $found = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -find "VC\Tools\MSVC\**\bin\Hostx64\x64\$name" | Select-Object -First 1
        if ($found) { return $found }
    }
    throw "$name not found: install the Visual Studio Build Tools C++ workload"
}

$dl = Join-Path $native '.download'
New-Item -ItemType Directory -Force $dl | Out-Null

# --- transcribe.cpp ---------------------------------------------------------
$tcDir = Join-Path $native 'transcribe'
$stamp = Join-Path $tcDir '.version'
if ($Force -or -not (Test-Path $stamp) -or (Get-Content $stamp) -ne $tcVersion) {
    if (-not $tcSha256) { throw "no sha256 pinned for transcribe.cpp $tcVersion" }
    $asset = "transcribe-native-$tcVersion-windows-x86_64-cpu-vulkan.tar.gz"
    $tgz = Join-Path $dl $asset
    Get-Verified "https://github.com/handy-computer/transcribe.cpp/releases/download/v$tcVersion/$asset" $tgz $tcSha256

    if (Test-Path $tcDir) { Remove-Item -Recurse -Force $tcDir }
    $tmp = Join-Path $dl 'tc-extract'
    if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
    New-Item -ItemType Directory -Force $tmp | Out-Null
    & tar.exe -xzf $tgz -C $tmp
    if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
    $bundle = Get-ChildItem $tmp -Directory | Select-Object -First 1

    $bin = Join-Path $tcDir 'bin'
    $lib = Join-Path $tcDir 'lib'
    New-Item -ItemType Directory -Force $bin, $lib | Out-Null
    Copy-Item (Join-Path $bundle.FullName '*.dll') $bin
    Copy-Item -Recurse (Join-Path $bundle.FullName 'licenses') (Join-Path $tcDir 'licenses')
    Copy-Item (Join-Path $bundle.FullName 'contract.json') $tcDir

    # Import library: the bundle ships DLLs only.
    $dumpbin = Find-MsvcTool 'dumpbin.exe'
    $libexe = Find-MsvcTool 'lib.exe'
    $exports = & $dumpbin /nologo /exports (Join-Path $bin 'transcribe.dll')
    $names = $exports | ForEach-Object {
        if ($_ -match '^\s+\d+\s+[0-9A-F]+\s+[0-9A-F]{8}\s+(\S+)') { $Matches[1] }
    }
    if ($names.Count -lt 50) { throw "unexpected export count $($names.Count)" }
    $def = Join-Path $lib 'transcribe.def'
    (@('LIBRARY transcribe.dll', 'EXPORTS') + $names) | Set-Content -Encoding ascii $def
    & $libexe /nologo /machine:x64 "/def:$def" "/out:$(Join-Path $lib 'transcribe.lib')" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'lib.exe failed' }

    # The manifest transcribe-cpp-sys reads on the TRANSCRIBE_DIR path.
    @{
        version       = $tcVersion
        commit        = ''
        shared        = $true
        backend_dl    = $true
        module_dir    = 'bin'
        backends      = @('cpu', 'vulkan')
        metal_embed   = $false
        include_dir   = 'include'
        lib_dir       = 'lib'
        libraries     = @('transcribe')
        library_paths = @()
        system_libs   = @()
        frameworks    = @()
        link_flags    = @()
    } | ConvertTo-Json | Set-Content -Encoding ascii (Join-Path $lib 'transcribe-link.json')
    New-Item -ItemType Directory -Force (Join-Path $tcDir 'include') | Out-Null
    Set-Content -Encoding ascii $stamp $tcVersion
    Write-Host "transcribe.cpp $tcVersion -> $tcDir ($($names.Count) exports)"
}

# --- llama.cpp --------------------------------------------------------------
$llDir = Join-Path $native 'llama'
$llStamp = Join-Path $llDir '.version'
if ($Force -or -not (Test-Path $llStamp) -or (Get-Content $llStamp) -ne $llamaBuild) {
    $asset = "llama-$llamaBuild-bin-win-vulkan-x64.zip"
    $zip = Join-Path $dl $asset
    Get-Verified "https://github.com/ggml-org/llama.cpp/releases/download/$llamaBuild/$asset" $zip $llamaSha256
    if (Test-Path $llDir) { Remove-Item -Recurse -Force $llDir }
    $tmp = Join-Path $dl 'll-extract'
    if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
    Expand-Archive $zip $tmp
    New-Item -ItemType Directory -Force $llDir | Out-Null
    $keep = @('llama.dll', 'ggml.dll', 'ggml-base.dll', 'ggml-vulkan.dll', 'libomp.dll', 'LICENSE-LLVM-OpenMP')
    Get-ChildItem $tmp -Recurse -File | Where-Object {
        $keep -contains $_.Name -or $_.Name -like 'ggml-cpu-*.dll'
    } | Copy-Item -Destination $llDir
    # Side-by-side manifest: llama.cpp's ggml.dll/ggml-base.dll share their
    # names with transcribe.cpp's (a different ggml build). Loading llama.dll
    # inside this activation context resolves its imports to these copies.
    $files = (Get-ChildItem $llDir -Filter *.dll | ForEach-Object { "  <file name=`"$($_.Name)`"/>" }) -join "`n"
    @"
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="chatty.llama" version="1.0.0.0" processorArchitecture="amd64"/>
$files
</assembly>
"@ | Set-Content -Encoding utf8 (Join-Path $llDir 'llama.manifest')
    Set-Content -Encoding ascii $llStamp $llamaBuild
    Write-Host "llama.cpp $llamaBuild -> $llDir"
}
