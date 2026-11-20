# S4-01: the delivery zip postman-<version>-windows-x64.zip: postman.exe, postman-worker.exe, README.txt, install.ps1,
# postman.portable (the self-update flag), postman-viewer.exe (S8-24, ADR 0007) and postman-plugins.exe (the common
# plugin set, ADR 0013: mail and directory together). The channel components (PLG-17, owner decision 2026-11-19
# "components") are NOT in the zip: the app downloads their set files from the release (PLG-15), and they are
# published as their own zips - postman-plugins-<channel>-windows-x64.zip - the same files the app unpacks into the
# components folder; without the network an administrator hands them over and postman --install-component installs
# them (PLG-16). WASM viewer modules (viewers/*) are not packaged.
# The MSI postman-<version>-windows-x64.msi (owner decision 2026-11-07) - the base files plus the channel set files
# as optional features, off by default and turned on by CHANNELS (PLG-13); made by postman-pack.exe of the same
# build (tools/pack); signed afterwards by scripts\windows\sign.ps1 in CI.
# Takes the exes of a finished release build (cargo build --release --workspace --features postman-worker/viewer);
# the version is the one built into postman.exe (Cargo). Prints the zip path; in GitHub Actions also sets the
# outputs name, dir (the folder with the packaged files, the build artifact), version, msi (its path) and assets
# (S8-39: the files of a GitHub release - the zip, the MSI and the components under their own names,
# docs/DEV.md "Выпуск").
# Usage: powershell -File scripts\windows\package.ps1 [-TargetDir <cargo target dir>] [-Out <dir, default dist>]
param(
  [string]$TargetDir = "",
  [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$root = [IO.Path]::GetFullPath("$PSScriptRoot\..\..")
# Build directory from cargo (honours CARGO_TARGET_DIR) - never a hard-coded target\ (H-01).
if (-not $TargetDir) {
  $TargetDir = (cargo metadata --format-version 1 --no-deps --manifest-path "$root\Cargo.toml" | ConvertFrom-Json).target_directory
}
if (-not $Out) { $Out = Join-Path $root "dist" }
# Relative paths (CI: CARGO_TARGET_DIR=target) - from the current directory, absolute in the outputs.
$TargetDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($TargetDir)
$Out = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Out)
$release = Join-Path $TargetDir "release"
$exe = Join-Path $release "postman.exe"
if (-not (Test-Path -LiteralPath $exe)) { throw "no $exe - run cargo build --release --workspace --features postman-worker/viewer first" }

$version = (Get-Item -LiteralPath $exe).VersionInfo.ProductVersion
if (-not $version) { throw "postman.exe has no version resource (app-gpui/build.rs)" }
$name = "postman-$version-windows-x64"
$dir = Join-Path $Out $name
$zip = Join-Path $Out "$name.zip"
if (Test-Path -LiteralPath $dir) { Remove-Item -LiteralPath $dir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Copy-Item -LiteralPath $exe, (Join-Path $release "postman-worker.exe") -Destination $dir
# The viewer host lies next to the worker; only a build with the worker feature `viewer` has it.
$viewer = Join-Path $release "postman-viewer.exe"
if (-not (Test-Path -LiteralPath $viewer)) { throw "no $viewer - build with --features postman-worker/viewer" }
Copy-Item -LiteralPath $viewer -Destination $dir
# The plugin set (PLG-03, ADR 0013): built by the same workspace call; the host looks for it next to postman.exe.
$plugins = Join-Path $release "postman-plugins.exe"
if (-not (Test-Path -LiteralPath $plugins)) { throw "no $plugins - build the whole workspace (cargo build --release --workspace)" }
Copy-Item -LiteralPath $plugins -Destination $dir
# The Telegram set file (PLG-05) and the Matrix one (PLG-04): the MSI cabinet carries them (optional features,
# PLG-13); the zip does not (PLG-17).
$pluginsTelegram = Join-Path $release "postman-plugins-telegram.exe"
if (-not (Test-Path -LiteralPath $pluginsTelegram)) { throw "no $pluginsTelegram - build the whole workspace (cargo build --release --workspace)" }
Copy-Item -LiteralPath $pluginsTelegram -Destination $dir
$pluginsMatrix = Join-Path $release "postman-plugins-matrix.exe"
if (-not (Test-Path -LiteralPath $pluginsMatrix)) { throw "no $pluginsMatrix - build the whole workspace (cargo build --release --workspace)" }
Copy-Item -LiteralPath $pluginsMatrix -Destination $dir
Copy-Item -LiteralPath "$PSScriptRoot\README.txt", "$PSScriptRoot\install.ps1" -Destination $dir
# Portable flag (self-update, decision 2026-11-11): next to postman.exe the update takes the zip, not the MSI.
# The MSI lists its files itself (tools/pack ITEMS) and does not install the flag.
Set-Content -LiteralPath (Join-Path $dir "postman.portable") -Value "Postman portable: updates replace these files from the zip." -Encoding ascii
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
# The zip without the channel set files (PLG-17): the base kit only - the MSI still carries both (its cabinet
# is made from the whole $dir).
Compress-Archive -Path (Join-Path $dir "postman.exe"), (Join-Path $dir "postman-worker.exe"), (Join-Path $dir "postman-viewer.exe"), (Join-Path $dir "postman-plugins.exe"), (Join-Path $dir "README.txt"), (Join-Path $dir "install.ps1"), (Join-Path $dir "postman.portable") -DestinationPath $zip -CompressionLevel Optimal
# Release files: the zip, the components a user can download alone ("Component missing", S8-38) and the channel
# components as their own zips (PLG-17): what the app downloads (PLG-15) and what --install-component takes
# offline (PLG-16); signed by minisign in CI like the rest.
$assets = Join-Path $Out "assets"
if (Test-Path -LiteralPath $assets) { Remove-Item -LiteralPath $assets -Recurse -Force }
New-Item -ItemType Directory -Force -Path $assets | Out-Null
foreach ($set in "postman-plugins-telegram", "postman-plugins-matrix") {
  $component = Join-Path $assets "$set-windows-x64.zip"
  if (Test-Path -LiteralPath $component) { Remove-Item -LiteralPath $component -Force }
  Compress-Archive -Path (Join-Path $dir "$set.exe") -DestinationPath $component -CompressionLevel Optimal
}
Copy-Item -LiteralPath $zip, (Join-Path $dir "postman-worker.exe"), (Join-Path $dir "postman-viewer.exe") -Destination $assets
$pack = Join-Path $release "postman-pack.exe"
if (-not (Test-Path -LiteralPath $pack)) { throw "no $pack - build the whole workspace (cargo build --release --workspace)" }
$msi = Join-Path $assets "$name.msi"
& $pack msi $dir $msi
if ($LASTEXITCODE) { throw "postman-pack msi failed ($LASTEXITCODE)" }
if ($env:GITHUB_OUTPUT) {
  # UTF-8 without BOM in Windows PowerShell 5.1 and in pwsh alike.
  $lines = "name=$name`ndir=$dir`nassets=$assets`nmsi=$msi`nversion=$version`n"
  [IO.File]::AppendAllText($env:GITHUB_OUTPUT, $lines, (New-Object Text.UTF8Encoding $false))
}
"package: $zip"
"package: $msi"
