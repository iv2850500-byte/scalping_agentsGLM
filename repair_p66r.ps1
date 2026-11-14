 $ErrorActionPreference = "Stop"
 $root = $PSScriptRoot
if (-not $root) { $root = "C:\Users\BASE_PC\scalping_agentsGLM" }
Set-Location $root
Write-Host "[P66R] root = $root"
function Count-Literal([string]$t,[string]$n){ ([regex]::Matches($t,[regex]::Escape($n))).Count }
if (-not (Test-Path (Join-Path $root "Cargo.toml"))) { Write-Host "[P66R] ERROR: Cargo.toml not found - wrong root" -ForegroundColor Red; exit 1 }
 $date = Get-Date -Format "yyyy-MM-dd"
 $script:missing = $false
function Repair([string]$rel){
  $src = Join-Path $root ("patched\" + $rel)
  if (-not (Test-Path $src)) { Write-Host ("[P66R] MISSING SOURCE: {0}" -f $src) -ForegroundColor Red; $script:missing = $true; return }
  $dst = Join-Path $root $rel
  $bdir = Join-Path $root ("backups\" + $date)
  New-Item -ItemType Directory -Force -Path $bdir | Out-Null
  if (Test-Path $dst) { Copy-Item $dst (Join-Path $bdir ((Split-Path $rel -Leaf) + ".bak-REPAIR")) -Force }
  New-Item -ItemType Directory -Force -Path (Split-Path $dst -Parent) | Out-Null
  Copy-Item $src $dst -Force
  Write-Host ("[P66R] repaired: {0}" -f $rel)
}
Repair "src\strategies\momentum.rs"
Repair "src\utils\mod.rs"
Repair "src\utils\app_error.rs"
Repair "src\main.rs"
Repair "src\bin\wfa_pipeline.rs"
Repair "src\bin\threshold_grid.rs"
if ($script:missing) {
  Write-Host "[P66R] ABORT: patched\ sources incomplete. Put P64_pack.zip / P65_pack.zip / P66_pack.zip INTO the repo root and run:" -ForegroundColor Red
  Write-Host "  Expand-Archive -Path .\P64_pack.zip -DestinationPath . -Force"
  Write-Host "  Expand-Archive -Path .\P65_pack.zip -DestinationPath . -Force"
  Write-Host "  Expand-Archive -Path .\P66_pack.zip -DestinationPath . -Force"
  Write-Host "then re-run: powershell -ExecutionPolicy Bypass -File .\repair_p66r.ps1"
  exit 1
}
 $cg = Get-Content Cargo.toml -Raw
if ($cg -notmatch [regex]::Escape("thiserror")) { $cg = $cg.Replace("[dependencies]", "[dependencies]`r`nthiserror = `"1`""); Write-Host "[P66R] Cargo.toml: + thiserror" }
if ($cg -notmatch [regex]::Escape('name = "wfa_pipeline"')) { $cg = $cg.TrimEnd() + "`r`n`r`n# P64/: WFA conveyor (feature-gated, not in release)`r`n[[bin]]`r`nname = `"wfa_pipeline`"`r`npath = `"src/bin/wfa_pipeline.rs`"`r`nrequired-features = [`"wfa`"]`r`n"; Write-Host "[P66R] Cargo.toml: + [[bin]] wfa_pipeline" }
if ($cg -notmatch [regex]::Escape('name = "threshold_grid"')) { $cg = $cg.TrimEnd() + "`r`n`r`n# P66/: threshold grid (feature-gated, not in release)`r`n[[bin]]`r`nname = `"threshold_grid`"`r`npath = `"src/bin/threshold_grid.rs`"`r`nrequired-features = [`"wfa`"]`r`n"; Write-Host "[P66R] Cargo.toml: + [[bin]] threshold_grid" }
[System.IO.File]::WriteAllText((Join-Path $root "Cargo.toml"), $cg, (New-Object System.Text.UTF8Encoding($false)))
 $t64=0; $t65=0; $t66=0
Get-ChildItem src -Recurse -Filter *.rs | ForEach-Object { $s = Get-Content $_.FullName -Raw; $t64 += Count-Literal $s "P64/"; $t65 += Count-Literal $s "P65/"; $t66 += Count-Literal $s "P66/" }
 $cg2 = Get-Content Cargo.toml -Raw
 $t64 += Count-Literal $cg2 "P64/"; $t66 += Count-Literal $cg2 "P66/"
Write-Host ("[P66R] markers: P64={0} (exp 51) | P65={1} (exp 6) | P66={2} (exp 22)" -f $t64,$t65,$t66)
if ($t64 -lt 51 -or $t65 -lt 6 -or $t66 -lt 22) { Write-Host "[P66R] INCOMPLETE - send the FULL console output back" -ForegroundColor Red; exit 1 }
Write-Host "[P66R] REPAIR OK. Next:"
Write-Host "  powershell -ExecutionPolicy Bypass -File .\build.ps1      # expect: 80 tests"
Write-Host "  powershell -ExecutionPolicy Bypass -File .\wfa_grid.ps1 -Days 450"
