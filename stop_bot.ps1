Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match "watchdog\.ps1" } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Get-Process scalping_agents -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 1
if (Get-Process scalping_agents -ErrorAction SilentlyContinue) { Write-Host "СТОП НЕ ПРОШЁЛ — процесс жив" } else { Write-Host "Бот остановлен (жёстко, хвосты watchdog зачищены)." }
