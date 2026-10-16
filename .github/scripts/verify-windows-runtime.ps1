param([Parameter(Mandatory)][string[]]$Binary)
$ErrorActionPreference = 'Stop'
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$hostDirectory = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { 'Hostarm64' } else { 'Hostx64' }
$dumpbin = Get-ChildItem "$installation\VC\Tools\MSVC\*\bin\$hostDirectory\*\dumpbin.exe" -ErrorAction SilentlyContinue |
  Sort-Object FullName -Descending | Select-Object -First 1
if (!$dumpbin -and $hostDirectory -eq 'Hostarm64') {
  $dumpbin = Get-ChildItem "$installation\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" |
    Sort-Object FullName -Descending | Select-Object -First 1
}
if (!$dumpbin) { throw 'dumpbin not found' }
foreach ($file in $Binary) {
  if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing binary: $file" }
  $dependencies = & $dumpbin.FullName /dependents $file
  if ($LASTEXITCODE -ne 0) { throw "Cannot inspect $file" }
  # VC-LTL uses the OS-provided msvcrt.dll for Win7 compatibility.
  if ($dependencies -match '(?i)(VCRUNTIME|MSVCP|MSVCR[0-9]|ucrtbase|api-ms-win-crt-).*\.dll') {
    throw "$file requires an external CRT runtime"
  }
}
