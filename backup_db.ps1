# backup_db.ps1 - P26 ops pack (roadmap #8). Copy SQLite DB + config TOMLs to
# backups\<timestamp>\. NEVER copies .env - secrets stay local and unencrypted.
# Retention: keeps 14 newest backup folders.
# Scheduled task (daily): see README_FIXPACK_P26_OPS.md
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root
$stamp = Get-Date -Format 'yyyyMMdd_HHmmss'
$dest = Join-Path $root ('backups\' + $stamp)
New-Item -ItemType Directory -Force -Path $dest | Out-Null
$copied = 0

# 1) DB files in project root
$rootDb = Get-ChildItem $root -File -ErrorAction SilentlyContinue | Where-Object { $_.Extension -in '.db','.sqlite','.sqlite3' }
foreach ($f in $rootDb) { Copy-Item $f.FullName -Destination $dest; $copied++ }

# 2) DB files in data\
$dataDir = Join-Path $root 'data'
if (Test-Path $dataDir) {
    $dataDb = Get-ChildItem $dataDir -File -Recurse -ErrorAction SilentlyContinue | Where-Object { $_.Extension -in '.db','.sqlite','.sqlite3' }
    foreach ($f in $dataDb) { Copy-Item $f.FullName -Destination $dest; $copied++ }
}

# 3) configs: root config*.toml + src\config\bots.toml
$cfgs = Get-ChildItem $root -Filter 'config*.toml' -File -ErrorAction SilentlyContinue
foreach ($f in $cfgs) { Copy-Item $f.FullName -Destination $dest; $copied++ }
if (Test-Path (Join-Path $root 'src\config\bots.toml')) {
    Copy-Item (Join-Path $root 'src\config\bots.toml') -Destination $dest
    $copied++
}

Write-Host ('[backup] ' + $copied + ' files -> ' + $dest)

# 4) retention: keep 14 newest folders in backups\
$bk = Join-Path $root 'backups'
$dirs = Get-ChildItem $bk -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending
if ($dirs.Count -gt 14) {
    $drop = $dirs | Select-Object -Skip 14
    foreach ($d in $drop) { Remove-Item $d.FullName -Recurse -Force }
    Write-Host ('[backup] retention: removed ' + $drop.Count + ' old folders')
}
