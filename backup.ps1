# backup.ps1 v2.1 - DBs + configs (recursive), 14 days, TG upload. ASCII-only, PS 5.1.
 $root = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
 $log  = Join-Path $root 'backups\backup.log'
 $secondDest = ''   # optional ANOTHER disk/USB path; empty = off

New-Item -ItemType Directory -Force -Path (Join-Path $root 'backups') | Out-Null
function Write-Log([string]$msg) {
    $line = ('[{0}] {1}' -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $msg)
    $line | Out-File -FilePath $log -Append -Encoding utf8
}

 $dest = Join-Path $root ('backups\' + (Get-Date -Format 'yyyy-MM-dd'))
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Write-Log ('start -> ' + $dest)

 $copied = 0; $failed = 0

Get-ChildItem $root -Recurse -Include '*.db','*.db-wal','*.db-shm' -File -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -notmatch '\\backups\\' } |
    ForEach-Object {
        try { Copy-Item $_.FullName $dest -Force -ErrorAction Stop; $copied++ }
        catch { $failed++; Write-Log ('FAIL db: ' + $_.Name + ' : ' + $_.Exception.Message) }
    }

# FIX v2.1: configs recursive now (bots.toml lives in src\config\)
Get-ChildItem $root -Recurse -Include '*.toml' -File -ErrorAction SilentlyContinue |
    Where-Object {
        $_.FullName -notmatch '\\backups\\' -and
        $_.FullName -notmatch '\\target\\' -and
        $_.FullName -notmatch '\\\.git\\' -and
        ($_.Name -like 'config*.toml' -or $_.Name -like 'bots.toml')
    } |
    ForEach-Object {
        try { Copy-Item $_.FullName $dest -Force -ErrorAction Stop; $copied++ }
        catch { $failed++; Write-Log ('FAIL cfg: ' + $_.Name + ' : ' + $_.Exception.Message) }
    }

if ($copied -le 0) { Write-Log 'RESULT: FAILED - nothing copied'; exit 1 }

if ($secondDest -ne '') {
    if (Test-Path $secondDest) {
        $dest2 = Join-Path $secondDest (Get-Date -Format 'yyyy-MM-dd')
        New-Item -ItemType Directory -Force -Path $dest2 | Out-Null
        Copy-Item (Join-Path $dest '*') $dest2 -Force -ErrorAction SilentlyContinue
        Write-Log ('second copy -> ' + $dest2)
        Get-ChildItem $secondDest -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending | Select-Object -Skip 14 |
            Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    } else { Write-Log ('WARN second dest missing: ' + $secondDest) }
}

# NEW v2.1: TG upload of day-zip (tokens from .env, never hardcoded)
 $tgToken = ''; $tgChat = ''
 $envPath = Join-Path $root '.env'
if (Test-Path $envPath) {
    Get-Content $envPath | ForEach-Object {
        if ($_ -match '^\s*TG_BOT_TOKEN\s*=\s*(.+?)\s*$') { $tgToken = $Matches[1].Trim('"') }
        if ($_ -match '^\s*TG_CHAT_ID\s*=\s*(.+?)\s*$')   { $tgChat  = $Matches[1].Trim('"') }
    }
}
if ($tgToken -ne '' -and $tgChat -ne '') {
    $fs = $null
    try {
        $zip = Join-Path $root ('backups\' + (Get-Date -Format 'yyyy-MM-dd') + '.zip')
        if (Test-Path $zip) { Remove-Item $zip -Force }
        Compress-Archive -Path (Join-Path $dest '*') -DestinationPath $zip -Force
        if ((Get-Item $zip).Length -lt 48MB) {
            Add-Type -AssemblyName System.Net.Http; $hc = New-Object System.Net.Http.HttpClient
            $hc.Timeout = [TimeSpan]::FromMinutes(5)
            $mp = New-Object System.Net.Http.MultipartFormDataContent
            $fs = [System.IO.File]::OpenRead($zip)
            $mp.Add((New-Object System.Net.Http.StreamContent($fs)), 'document', (Split-Path $zip -Leaf))
            $mp.Add((New-Object System.Net.Http.StringContent($tgChat)), 'chat_id')
            $resp = $hc.PostAsync(('https://api.telegram.org/bot' + $tgToken + '/sendDocument'), $mp).Result
            $fs.Close()
            if ($resp.IsSuccessStatusCode) { Write-Log ('TG upload OK: ' + (Split-Path $zip -Leaf)) }
            else { Write-Log ('TG upload FAIL: HTTP ' + [int]$resp.StatusCode) }
        } else { Write-Log 'TG upload SKIP: zip > 48MB' }
    } catch {
        Write-Log ('TG upload FAIL: ' + $_.Exception.Message)
        if ($fs -ne $null) { $fs.Close() }
    }
} else { Write-Log 'TG upload SKIP: no TG_BOT_TOKEN/TG_CHAT_ID in .env' }

Get-ChildItem (Join-Path $root 'backups') -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -Skip 14 |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
Get-ChildItem (Join-Path $root 'backups') -Filter '*.zip' -File -ErrorAction SilentlyContinue |
    Where-Object { $_.LastWriteTime -lt (Get-Date).AddDays(-60) } |
    Remove-Item -Force -ErrorAction SilentlyContinue

Write-Log ('RESULT: OK files=' + $copied + ' failed=' + $failed + ' -> ' + $dest)
