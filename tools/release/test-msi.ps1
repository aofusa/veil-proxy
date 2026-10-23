# F-252: MSI を入れて、既定の設定で動くか・上げる・消すを確かめる（windows の runner。管理者で動く）。
#
#   pwsh tools/release/test-msi.ps1 -Msi <msi> -Version 0.9.0
param([Parameter(Mandatory)][string]$Msi, [Parameter(Mandatory)][string]$Version)
$ErrorActionPreference = 'Stop'
$fails = 0
function Ok($m) { Write-Host "ok   $m" }
function Bad($m) { Write-Host "FAIL $m"; Write-Host "::error title=msi test::$m"; $script:fails++ }
$msiPath = (Resolve-Path $Msi).Path
$data = Join-Path $env:ProgramData 'veil'
$exe = Join-Path $env:ProgramFiles 'veil\veil.exe'

function Install-Msi { $p = Start-Process msiexec -ArgumentList "/i `"$msiPath`" /qn /l*v `"$env:RUNNER_TEMP\msi.log`"" -Wait -PassThru; return $p.ExitCode }
function Uninstall-Msi { $p = Start-Process msiexec -ArgumentList "/x `"$msiPath`" /qn" -Wait -PassThru; return $p.ExitCode }
function Check-Serving($label) {
  $job = Start-Process -FilePath $exe -PassThru -NoNewWindow -RedirectStandardOutput "$env:RUNNER_TEMP\veil.out" -RedirectStandardError "$env:RUNNER_TEMP\veil.err"
  $up = $false
  for ($i = 0; $i -lt 60; $i++) { try { Invoke-WebRequest -UseBasicParsing http://127.0.0.1:8080/ -TimeoutSec 2 | Out-Null; $up = $true; break } catch { Start-Sleep 1 } }
  if ($up) {
    foreach ($p in '/', '/quickstart.html') {
      try { $r = Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:8080$p"; if ($r.StatusCode -eq 200) { Ok "$label GET $p" } else { Bad "$label GET $p -> $($r.StatusCode)" } } catch { Bad "$label GET $p -> $_" }
    }
  } else { Bad "$label did not start"; Get-Content "$env:RUNNER_TEMP\veil.err","$env:RUNNER_TEMP\veil.out" -ErrorAction SilentlyContinue | Select-Object -Last 30 | Write-Host }
  # アクセスログは非同期に書き出される。止める前に出るのを待つ
  for ($i = 0; $i -lt 15; $i++) { if ((Get-Content "$env:RUNNER_TEMP\veil.out" -Raw -ErrorAction SilentlyContinue) -match 'type=access') { break }; Start-Sleep 1 }
  Stop-Process -Id $job.Id -Force -ErrorAction SilentlyContinue; Start-Sleep 1
  $out = Get-Content "$env:RUNNER_TEMP\veil.out" -Raw -ErrorAction SilentlyContinue
  if ($out -match [regex]::Escape("Config File: $data\config.toml (from default path)")) { Ok "$label reads %ProgramData%\veil\config.toml from the default path" } else { Bad "$label did not read the default path" }
  if ($out -match 'type=access') { Ok "$label access log on stdout" } else { Bad "$label no access log" }
}

# 1. 新しく入れる
$rc = Install-Msi; if ($rc -eq 0) { Ok "install" } else { Bad "install exit $rc"; Get-Content "$env:RUNNER_TEMP\msi.log" -Tail 60 | Write-Host }
foreach ($f in "$exe", "$data\config.toml", "$data\www\index.html", "$data\www\quickstart.html") { if (Test-Path $f) { Ok "installed $f" } else { Bad "missing $f" } }
$machinePath = [Environment]::GetEnvironmentVariable('PATH', 'Machine')
if ($machinePath -like "*$env:ProgramFiles\veil*") { Ok "PATH has the install dir" } else { Bad "PATH lacks the install dir" }
$v = & $exe --version; if ($v -match [regex]::Escape($Version)) { Ok "--version $v" } else { Bad "--version $v (want $Version)" }
$acl = Get-Acl $data
# Users・Authenticated Users・Everyone・CREATOR OWNER が書き込めないこと（継承した許可も含めて）
$weak = $acl.Access | Where-Object {
  $_.AccessControlType -eq 'Allow' -and $_.IdentityReference -match 'Users$|Everyone|Authenticated Users|CREATOR OWNER' -and
  ($_.FileSystemRights.ToString() -match 'Write|Modify|FullControl|CreateFiles|AppendData|Delete')
}
if (-not $weak) { Ok "only administrators can change %ProgramData%\veil" }
else { Bad "%ProgramData%\veil is writable by: $(($weak | ForEach-Object { "$($_.IdentityReference)=$($_.FileSystemRights)" }) -join ', ')" }
if ($acl.AreAccessRulesProtected) { Ok "%ProgramData%\veil does not inherit permissions" } else { Bad "%ProgramData%\veil inherits permissions" }
Check-Serving "fresh install:"

# 2. 書き換えて入れ直す（上げるのと同じ扱い）
Add-Content "$data\config.toml" "`n# edited by test"
$p = Start-Process msiexec -ArgumentList "/fa `"$msiPath`" /qn" -Wait -PassThru
if ((Get-Content "$data\config.toml" -Raw) -match 'edited by test') { Ok "repair keeps the edited configuration" } else { Bad "repair overwrote the configuration" }

# 3. 消す
$rc = Uninstall-Msi; if ($rc -eq 0) { Ok "uninstall" } else { Bad "uninstall exit $rc" }
if (-not (Test-Path $exe)) { Ok "uninstall removes veil.exe" } else { Bad "veil.exe left after uninstall" }
if (Test-Path "$data\config.toml") { Ok "uninstall keeps the configuration" } else { Bad "uninstall deleted the configuration" }
$machinePath = [Environment]::GetEnvironmentVariable('PATH', 'Machine')
if ($machinePath -notlike "*$env:ProgramFiles\veil*") { Ok "uninstall removes the PATH entry" } else { Bad "PATH entry left" }

# 4. 前の設定がある状態で入れ直すと残る
$rc = Install-Msi
if ((Get-Content "$data\config.toml" -Raw) -match 'edited by test') { Ok "reinstall keeps an existing configuration" } else { Bad "reinstall replaced an existing configuration" }
Uninstall-Msi | Out-Null

if ($fails -gt 0) { Write-Host "MSI TEST: $fails failure(s)"; exit 1 }
Write-Host "MSI TEST: ALL OK"
