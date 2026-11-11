# Builds Aero and installs it for the current user:
#   - copies aero.exe to %LOCALAPPDATA%\Programs\Aero
#   - creates a Start Menu shortcut "Aero" (search for it in Start)
#   - restarts Aero if it was running
# Run it again after changing the code to update your installed copy.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

# Make cargo findable even in shells opened before Rust was installed (Scoop's rustup-gnu).
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $scoop = "$env:USERPROFILE\scoop"
    $env:CARGO_HOME = "$scoop\persist\rustup-gnu\.cargo"
    $env:RUSTUP_HOME = "$scoop\persist\rustup-gnu\.rustup"
    $env:PATH = "$env:CARGO_HOME\bin;$scoop\apps\mingw\current\bin;$env:PATH"
}

$dest = Join-Path $env:LOCALAPPDATA "Programs\Aero"
$exe = Join-Path $dest "aero.exe"
New-Item -ItemType Directory -Force $dest | Out-Null

# Close Aero first: a running copy locks its exe (installed or in target\release).
$running = Get-Process aero -ErrorAction SilentlyContinue
$runningPath = if ($running) { ($running | Select-Object -First 1).Path }
if ($running) {
    Write-Host "Closing the running Aero..."
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 800
}

Write-Host "Building Aero..."
cargo build --release
if ($LASTEXITCODE -ne 0) {
    if ($runningPath) { Start-Process $runningPath -ArgumentList "--tray" }
    throw "Build failed (the previous Aero was restarted)."
}

Copy-Item "target\release\aero.exe" $exe -Force
Write-Host "Installed to $exe"

# If "Start with Windows" is on, make sure it starts this installed copy.
$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
if (Get-ItemProperty $runKey -Name Aero -ErrorAction SilentlyContinue) {
    Set-ItemProperty $runKey -Name Aero -Value "`"$exe`" --tray"
}

$lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "Aero.lnk"
$shell = New-Object -ComObject WScript.Shell
$sc = $shell.CreateShortcut($lnk)
$sc.TargetPath = $exe
$sc.WorkingDirectory = $dest
$sc.IconLocation = "$exe,0"
$sc.Description = "Aero - instant replay clipper"
$sc.Save()
Write-Host "Start Menu shortcut: $lnk"

if ($running) {
    Start-Process $exe -ArgumentList "--tray"
    Write-Host "Aero restarted (in the tray)."
}
Write-Host "Done. Search for 'Aero' in the Start Menu."
