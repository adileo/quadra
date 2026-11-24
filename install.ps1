# Installs quadra on Windows:
#
#   irm https://quadra.run/install.ps1 | iex
#
# It downloads the official binary from the releases of github.com/adileo/quadra, checks its
# SHA-256, puts it in %USERPROFILE%\.quadra\bin and adds that folder to your user PATH.
#
#   $env:QUADRA_VERSION = "0.5.0"   a version instead of the latest
#   $env:QUADRA_INSTALL = "C:\..."  the folder (default %USERPROFILE%\.quadra)
#   $env:QUADRA_NO_MODIFY_PATH = 1  leave the PATH alone
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$releases = if ($env:QUADRA_RELEASES_URL) { $env:QUADRA_RELEASES_URL } else { 'https://github.com/adileo/quadra/releases' }
$home_dir = if ($env:QUADRA_INSTALL) { $env:QUADRA_INSTALL } else { Join-Path $env:USERPROFILE '.quadra' }
$bin_dir = Join-Path $home_dir 'bin'

$arch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
switch ($arch) {
  'AMD64' { $target = 'x86_64-pc-windows-msvc' }
  # x64 binaries run on Windows on ARM
  'ARM64' { $target = 'x86_64-pc-windows-msvc' }
  default { throw "No official binary for the processor $arch (build from the sources: https://github.com/adileo/quadra)" }
}

$base = if ($env:QUADRA_VERSION) { "$releases/download/v$($env:QUADRA_VERSION.TrimStart('v'))" } else { "$releases/latest/download" }
$asset = "quadra-$target.gz"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("quadra-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Write-Host "Downloading quadra ($target)..."
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset)
  Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile (Join-Path $tmp "$asset.sha256")
  $want = ((Get-Content (Join-Path $tmp "$asset.sha256") -Raw).Trim() -split '\s+')[0].ToLower()
  $got = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLower()
  if ($want -ne $got) { throw "The download does not match its SHA-256 ($got, expected $want): nothing was installed" }

  $gz = [System.IO.File]::OpenRead((Join-Path $tmp $asset))
  $out = [System.IO.File]::Create((Join-Path $tmp 'quadra.exe'))
  $unzip = New-Object System.IO.Compression.GZipStream($gz, [System.IO.Compression.CompressionMode]::Decompress)
  $unzip.CopyTo($out)
  $unzip.Close(); $out.Close(); $gz.Close()

  New-Item -ItemType Directory -Force -Path $bin_dir | Out-Null
  $exe = Join-Path $bin_dir 'quadra.exe'
  # a quadra running now keeps its file: move it aside, it is removed at its next start
  if (Test-Path $exe) {
    $old = Join-Path $bin_dir 'quadra.old.exe'
    Remove-Item -Force $old -ErrorAction SilentlyContinue
    Move-Item -Force $exe $old
  }
  Move-Item -Force (Join-Path $tmp 'quadra.exe') $exe
  Write-Host "Installed $(& $exe --version) in $exe"
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$user_path = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = @(); if ($user_path) { $parts = $user_path -split ';' | Where-Object { $_ } }
if ($parts -notcontains $bin_dir) {
  if ($env:QUADRA_NO_MODIFY_PATH) {
    Write-Host "Add $bin_dir to your PATH to run quadra."
  } else {
    [Environment]::SetEnvironmentVariable('Path', (($parts + $bin_dir) -join ';'), 'User')
    $env:Path = "$env:Path;$bin_dir"
    Write-Host "Added $bin_dir to your user PATH."
  }
}
Write-Host ""
Write-Host "Open a new terminal, then: quadra init my-app"
