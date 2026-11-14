# P48/log-retention tool. Deletes logs\bot.log.YYYY-MM-DD older than
# RETENTION_DAYS (default 14). Today log is never touched. Read-only on src.
param([int]$Days = 14)
Stop = 'Stop'
Set-Location 'C:\Users\BASE_PC\scalping_agentsGLM'
 = Join-Path (Get-Location) 'logs'
if (-not (Test-Path $logDir)) { Write-Host '[cleanup] no logs dir'; exit 0 }
$cut = (Get-Date).Date.AddDays(-$Days)
$freed = [long]0; $n = 0
Get-ChildItem $logDir -File | ForEach-Object {
    if ($_.Name -match '^bot\.log\.(\d{4}-\d{2}-\d{2})$') {
        $d = [datetime]::ParseExact($Matches[1], 'yyyy-MM-dd', $null)
        if ($d -lt $cut) {
            $freed += $_.Length; $n++
            Remove-Item $_.FullName -Force
            Write-Host ("[cleanup] deleted {0}  ({1:N0} KB)" -f $_.Name, ($_.Length/1KB))
        }
    }
}
Write-Host ("[cleanup] done: {0} file(s), freed {1:N1} MB (retention {2} days)" -f $n, ($freed/1MB), $Days)
