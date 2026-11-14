# ============================================================
#  rename_avrora.ps1   [AEGIS -> AVRORA]
#  1) switch $BOTNAME in deploy_aegis.ps1
#  2) pause watchdog/backup during the rebuild
#  Run: powershell -ExecutionPolicy Bypass -File .\rename_avrora.ps1
#  THEN: powershell -ExecutionPolicy Bypass -File .\deploy_aegis.ps1
# ============================================================
 $ErrorActionPreference = 'Stop'
 $f = 'C:\Users\BASE_PC\scalping_agentsGLM\deploy_aegis.ps1'
 $c = [IO.File]::ReadAllText($f)
 $q  = [char]39
 $old = '$BOTNAME = ' + $q + 'AEGIS' + $q
 $new = '$BOTNAME = ' + $q + 'AVRORA' + $q
if ($c.Contains($new)) { Write-Host '  [skip] already AVRORA' }
elseif ($c.Contains($old)) {
  $c = $c.Replace($old, $new)
  [IO.File]::WriteAllText($f, $c, [Text.Encoding]::ASCII)
  Write-Host '  [ok] deploy_aegis.ps1: BOTNAME AEGIS -> AVRORA'
} else { Write-Host '  [X] BOTNAME line not found in deploy_aegis.ps1 - nothing changed'; exit 1 }

 $ErrorActionPreference = 'SilentlyContinue'
Disable-ScheduledTask -TaskName 'ScalpWatchdog' | Out-Null
Disable-ScheduledTask -TaskName 'ScalpBackup' | Out-Null
Get-Process -Name scalping_agents | Stop-Process -Force
Write-Host '  [ok] watchdog/backup paused + no running instance'
Write-Host '  NEXT: powershell -ExecutionPolicy Bypass -File .\deploy_aegis.ps1'