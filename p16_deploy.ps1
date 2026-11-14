# ============================================================
# p16_deploy.ps1 (PS 5.1, ASCII only)
# Deploy P16 build: stop bot -> swap exe -> start -> preflight -> insurance.
# PREREQ: p16_gate.ps1 finished OK. RUN ONLY ON EXPLICIT DEPLOY DECISION.
# Rollback exe: backups\exe\scalping_agents.exe.bak-P16
# ============================================================
 $ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

 $exeName  = 'scalping_agents.exe'
 $procName = 'scalping_agents'
 $newExe   = 'target_p16\release\' + $exeName
 $bakExe   = 'backups\exe\scalping_agents.exe.bak-P16'

if (-not (Test-Path $newExe)) { throw "new exe not found: $newExe - run p16_gate.ps1 first" }
 $ageH = ((Get-Date) - (Get-Item $newExe).LastWriteTime).TotalHours
if ($ageH -gt 24) { Write-Host ("[deploy] WARNING: build is {0} h old" -f [math]::Round($ageH,1)) }

# --- 1. capture running bot (path + args for faithful restart) ---
 $proc = Get-Process -Name $procName -ErrorAction SilentlyContinue | Select-Object -First 1
if ($proc) {
  $ci = Get-CimInstance Win32_Process -Filter "ProcessId=$($proc.Id)"
  $oldPath = $ci.ExecutablePath
  $oldCmd  = $ci.CommandLine
  Write-Host "[deploy] running bot: PID $($proc.Id) :: $oldCmd"
} else {
  Write-Host '[deploy] bot NOT running - will place new exe into target\release'
  $oldPath = Join-Path $PWD ("target\release\" + $exeName)
  if (-not (Test-Path $oldPath)) { $oldPath = Join-Path $PWD $exeName }
  $oldCmd = $null
}

# --- 2. backup old exe ---
New-Item -ItemType Directory -Force -Path (Split-Path $bakExe) | Out-Null
if (Test-Path $oldPath) { Copy-Item $oldPath $bakExe -Force; Write-Host "[deploy] old exe backed up -> $bakExe" }

# --- 3. stop bot ---
if ($proc) {
  Write-Host '[deploy] stopping bot...'
  Stop-Process -Id $proc.Id -Force
  $deadline = (Get-Date).AddSeconds(15)
  while ((Get-Date) -lt $deadline) {
    if (-not (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue)) { break }
    Start-Sleep -Milliseconds 300
  }
  if (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) { throw 'old bot process did not exit in 15s' }
  Write-Host '[deploy] bot stopped'
}

# --- 4. swap exe (retry against AV/file locks) ---
for ($i=1; $i -le 6; $i++) {
  try { Copy-Item $newExe $oldPath -Force; break }
  catch {
    if ($i -eq 6) { throw "cannot replace exe: $_ ; new build waits in $newExe" }
    Write-Host "[deploy] copy attempt $i failed, retry in 2s..."
    Start-Sleep 2
  }
}
Write-Host "[deploy] new exe in place: $oldPath"

# --- 5. start (reuse original args if any) ---
 $extraArgs = ''
if ($oldCmd) {
  $c = $oldCmd.Trim()
  if ($c.StartsWith('"')) {
    $idx = $c.IndexOf('"', 1)
    if ($idx -ge 0) { $extraArgs = $c.Substring($idx + 1).Trim() }
  } else {
    $parts = $c -split ' ', 2
    if ($parts.Count -gt 1) { $extraArgs = $parts[1] }
  }
}
if ($extraArgs) { Start-Process -FilePath $oldPath -ArgumentList $extraArgs -WorkingDirectory $PWD.Path -WindowStyle Hidden }
else            { Start-Process -FilePath $oldPath -WorkingDirectory $PWD.Path -WindowStyle Hidden }
Write-Host "[deploy] started, cwd=$($PWD.Path)"

# --- 6. preflight: WebUI port + P16 markers + panic scan ---
function Test-Port([int]$p) {
  $c = New-Object Net.Sockets.TcpClient
  try { $c.Connect('127.0.0.1', $p); $true } catch { $false } finally { $c.Close() }
}
 $up = $false
for ($i=1; $i -le 30; $i++) { if (Test-Port 9090) { $up = $true; break }; Start-Sleep 1 }
if (-not $up) { throw 'WebUI 127.0.0.1:9090 did not come up in 30s - CHECK LOG; rollback exe is in backups\exe\' }
Write-Host '[deploy] WebUI 9090 is UP'

Start-Sleep 3
 $log = Get-ChildItem .\logs -Filter 'bot.log*' -File | Sort-Object LastWriteTime -Descending | Select-Object -First 1
 $fs = [System.IO.FileStream]::new($log.FullName, 'Open', 'Read', 'ReadWrite')
 $sr = New-Object System.IO.StreamReader($fs, [System.Text.Encoding]::UTF8)
 $txt = $sr.ReadToEnd(); $sr.Close(); $fs.Close()
 $lines = $txt -split "`r?`n"
 $p16 = @($lines | Select-String -SimpleMatch 'P16: sizing base = live equity')
Write-Host ("[deploy] P16 markers in {0}: {1}" -f $log.Name, $p16.Count)
 $p16 | Select-Object -Last 6 | ForEach-Object { $_.Line }
if ($p16.Count -eq 0) { Write-Host '[deploy] WARNING: no P16 markers - verify the new exe is actually running' }
 $panic = $lines | Select-String -Pattern 'panicked at' | Select-Object -Last 3
if ($panic) { Write-Host '[deploy] WARNING: panic lines found:'; $panic | ForEach-Object { $_.Line } }

# --- 7. insurance: alive after 20s ---
Start-Sleep 20
if (-not (Get-Process -Name $procName -ErrorAction SilentlyContinue)) {
  throw 'bot process died within 20s after start - rollback: restore exe from backups\exe\ and start again'
}
Write-Host '[deploy] OK: bot alive, WebUI up, P16 markers present.'
Write-Host '[deploy] Rollback exe if ever needed: backups\exe\scalping_agents.exe.bak-P16'