$ErrorActionPreference = "Stop"

$Dir = Join-Path $env:LOCALAPPDATA "conflux"
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Exe = Join-Path $Dir "conflux.exe"

Get-Process conflux -ErrorAction SilentlyContinue | Stop-Process -Force
Invoke-WebRequest -UseBasicParsing -OutFile $Exe `
  -Uri "https://github.com/Byntham/conflux/releases/latest/download/conflux-windows-x86_64.exe"

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not (($UserPath -split ";") -contains $Dir)) {
  [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dir", "User")
}
if (-not (($env:Path -split ";") -contains $Dir)) {
  $env:Path = "$env:Path;$Dir"
}

Write-Host "Installed conflux to $Exe."
& $Exe wait
