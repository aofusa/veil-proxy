# F-252: winget 用の MSI を WiX v5 で作る（windows の runner で回す）。
#
#   pwsh tools/release/build-msi.ps1 -Zip packaging/output/veil-<ver>-x86_64-pc-windows-msvc.zip -Arch x64 -Defaults <dir> -Out <msi>
#
# -Defaults は tools/release/render-defaults.sh winget <dir> の出力（config.toml と www/）。
param(
  [Parameter(Mandatory)] [string]$Zip,
  [Parameter(Mandatory)] [ValidateSet('x64','arm64')] [string]$Arch,
  [Parameter(Mandatory)] [string]$Defaults,
  [Parameter(Mandatory)] [string]$Out,
  [string]$WixVersion = '5.0.2'
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot/../.."
$version = (Select-String -Path "$root/Cargo.toml" -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
  dotnet tool install --global wix --version $WixVersion 2>&1 | Out-Host
  $env:PATH += ";$env:USERPROFILE\.dotnet\tools"
}
$work = Join-Path ([IO.Path]::GetTempPath()) ("veil-msi-" + [guid]::NewGuid())
Expand-Archive -Path $Zip -DestinationPath $work
$src = (Get-ChildItem $work -Directory | Select-Object -First 1).FullName
$log = wix build -arch $Arch `
  -d "Version=$version" -d "SourceDir=$src" -d "DefaultsDir=$((Resolve-Path $Defaults).Path)" `
  -o $Out "$root/packaging/windows/veil.wxs" 2>&1
$rc = $LASTEXITCODE
$log | Out-Host
if ($rc -ne 0) {
  # 公開リポジトリの注釈はログイン無しで読めるので、WiX のエラーを注釈に出す
  $log | Select-String -Pattern 'error|warning' | Select-Object -First 20 | ForEach-Object { Write-Host "::error title=wix ($Arch)::$($_.Line)" }
  throw "wix build failed ($rc)"
}
Remove-Item -Recurse -Force $work
Get-Item $Out | Format-List Name, Length
