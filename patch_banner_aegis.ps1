# ============================================================
#  patch_banner_aegis.ps1   (P15 finisher: rename banner)
#  Run: powershell -ExecutionPolicy Bypass -File .\patch_banner_aegis.ps1
# ============================================================
 $ErrorActionPreference = 'Stop'
 $root = 'C:\Users\BASE_PC\scalping_agentsGLM'
 $f    = Join-Path $root 'src\main.rs'

if (-not (Test-Path $f)) { Write-Host '  [X] main.rs not found'; exit 1 }
 $src = [IO.File]::ReadAllText($f)

Write-Host '============================================================'
Write-Host ' patch_banner_aegis  [P15 finisher]'
Write-Host '============================================================'

# -- 1) read-only report: P15 markers ------------------------
Write-Host '-- P15 markers in main.rs --'
 $checks = @(
  @('3d capped backoff',  'PATCH P15: fast retries first'),
  @('entry-gate',         'entries_allowed'),
  @('pnl hook',           'add_closed_pnl'),
  @('unlimited attempts', 'u32::MAX'),
  @('reject metric',      'SIGNALS_REJECTED')
)
foreach ($c in $checks) {
  $n = (Select-String -Path $f -Pattern $c[1] -SimpleMatch).Count
  $tag = 'ok'
  if ($n -eq 0) { $tag = 'MISSING' }
  Write-Host ('   [{0}] {1} = {2}' -f $tag, $c[0], $n)
}

# -- 2) banner replacement ------------------------------------
if ($src.Contains('AEGIS v{}')) { Write-Host '  [skip] banner already AEGIS'; exit 0 }

 $rocket  = [char]::ConvertFromUtf32(0x1F680)
 $newCall = 'tracing::info!("' + $rocket + ' AEGIS v{} | multi-agent futures scalper | isolated bots | risk-gates", env!("CARGO_PKG_VERSION"));'

 $rx = [regex]'(?m)^[ \t]*tracing::info!\("[^"]*P15 risk-gates[^"]*"\s*,[^\r\n]*\);'
 $ms = $rx.Matches($src)
if ($ms.Count -eq 0) { Write-Host '  [X] banner anchor not found - nothing written'; exit 1 }
if ($ms.Count -gt 1) { Write-Host ('  [X] anchor not unique: ' + $ms.Count + ' - nothing written'); exit 1 }

 $old = $ms[0].Value
Write-Host '  OLD:' $old

 $bakDir = Join-Path $root 'backup_p15'
New-Item -ItemType Directory -Force -Path $bakDir | Out-Null
[IO.File]::WriteAllText((Join-Path $bakDir 'main.rs.banner.bak'), $src, (New-Object System.Text.UTF8Encoding($false)))

 $out = $src.Remove($ms[0].Index, $old.Length).Insert($ms[0].Index, $newCall)
[IO.File]::WriteAllText($f, $out, (New-Object System.Text.UTF8Encoding($false)))
Write-Host '  [ok] banner written:'
Write-Host '  NEW:' $newCall
Write-Host '  backup: backup_p15\main.rs.banner.bak'