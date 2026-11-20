# CI check of the MSI (job `release`, the runner is elevated): silent install (channels are on by default,
# PLG-13), the channel set via CHANNELS, keeping the set across upgrades, refusal to downgrade, removal (with
# the AppContainer profiles of the worker and the plugin set, PAR-05, ADR 0013). Changes the machine - never run
# it on a workstation.
# The newer package is the same kit under the next patch version (postman-pack msi ... --as).
# Usage: pwsh -File scripts\windows\msi-check.ps1 -Msi <file.msi> -Kit <kit dir> -Pack <postman-pack.exe>
param(
  [Parameter(Mandatory)][string]$Msi,
  [Parameter(Mandatory)][string]$Kit,
  [Parameter(Mandatory)][string]$Pack
)
$ErrorActionPreference = "Stop"
$dir = Join-Path $env:ProgramFiles "Postman"
$lnk = Join-Path ([Environment]::GetFolderPath("CommonPrograms")) "Postman.lnk"
$eventKey = "HKLM:\SYSTEM\CurrentControlSet\Services\EventLog\Application\Postman"
$files = "postman.exe", "postman-worker.exe", "postman-viewer.exe", "postman-plugins.exe", "README.txt"
$logs = Join-Path $env:RUNNER_TEMP "msi-logs"
$profileNames = "Postman.MailWorker", "Postman.Plugins"
$mappings = "HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings"
# Channel set markers (PLG-13): the value named after the feature exists while the channel is installed.
$channelsKey = "HKLM:\SOFTWARE\Pets\Postman\Channels"
$channelFiles = "postman-plugins-telegram.exe", "postman-plugins-matrix.exe"
New-Item -ItemType Directory -Force $logs | Out-Null

function Msiexec([string]$what, [string]$name, [int[]]$ok = @(0, 3010)) {
  $log = Join-Path $logs "$name.log"
  $p = Start-Process msiexec.exe -ArgumentList "$what /qn /norestart /l*v `"$log`"" -Wait -PassThru
  if ($p.ExitCode -notin $ok) {
    Get-Content $log -Tail 60
    # Отказавшее действие и его причина: хвост попадает в дамп свойств — вытащить срез с «value 3».
    Select-String -Path $log -Pattern "value 3|error 1|error 2|CustomAction.*returned" -Context 4 | ForEach-Object {
      "--- $($_.LineNumber)"
      $_.Context.PreContext
      $_.Line
      $_.Context.PostContext
    } | Select-Object -First 80
    throw "msiexec $what -> $($p.ExitCode), expected $($ok -join '/')"
  }
  "msiexec $name -> $($p.ExitCode)"
}

# Postman entries in "Installed apps" (64-bit view).
function Entries {
  @(Get-ChildItem "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall" |
    Get-ItemProperty | Where-Object { $_.DisplayName -eq "Postman" -and $_.Publisher -eq "Pets" })
}

function Assert-Installed([string]$displayVersion) {
  foreach ($f in $files) { if (-not (Test-Path -LiteralPath (Join-Path $dir $f))) { throw "not installed: $f" } }
  $target = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath
  if ($target -ine (Join-Path $dir "postman.exe")) { throw "Start menu shortcut: '$target'" }
  # As written, not expanded: REG_EXPAND_SZ with %SystemRoot% (Get-ItemProperty would expand it).
  $k = Get-Item $eventKey
  $msg = $k.GetValue("EventMessageFile", $null, "DoNotExpandEnvironmentNames")
  if ($k.GetValueKind("EventMessageFile") -ne "ExpandString" -or $msg -notlike "%SystemRoot%\Microsoft.NET\Framework64\*EventLogMessages.dll") {
    throw "event source: $($k.GetValueKind('EventMessageFile')) '$msg'"
  }
  $e = Entries
  if ($e.Count -ne 1) { throw "expected one Postman entry in Installed apps, found $($e.Count)" }
  if ($e[0].DisplayVersion -ne $displayVersion) { throw "Installed apps version $($e[0].DisplayVersion), expected $displayVersion" }
  if ($e[0].InstallLocation.TrimEnd('\') -ine $dir) { throw "InstallLocation '$($e[0].InstallLocation)'" }
  "installed: $displayVersion, $dir, shortcut, event source"
}

# Product version of a package from its Property table.
function Msi-Version([string]$path) {
  $i = New-Object -ComObject WindowsInstaller.Installer
  $db = $i.OpenDatabase($path, 0)
  $v = $db.OpenView("SELECT Value FROM Property WHERE Property = 'ProductVersion'")
  # Execute writes to the pipeline: without [void] the function returns an array.
  [void]$v.Execute()
  $r = $v.Fetch()
  return [string]$r.StringData(1)
}

# The AppContainer profiles are registered for the current user (HKCU mappings, Moniker = name): the worker's and
# the plugin set's network one.
function Test-Profile([string]$name) {
  if (-not (Test-Path $mappings)) { return $false }
  [bool](Get-ChildItem $mappings | Where-Object { $_.GetValue("Moniker") -eq $name })
}

# Channel files on disk: "all" - both, "telegram" - Telegram only, "none" - no channels.
function Assert-Channels([string]$state) {
  $want = @{
    "postman-plugins-telegram.exe" = ($state -eq "all" -or $state -eq "telegram")
    "postman-plugins-matrix.exe"   = ($state -eq "all")
  }
  foreach ($f in $want.Keys) {
    $present = Test-Path -LiteralPath (Join-Path $dir $f)
    if ($present -ne $want[$f]) { throw "channel ${f}: present=$present, expected $($want[$f]) (state $state)" }
  }
  "channels: $state"
}

# Channel markers in the registry: the value exists while the channel component is installed.
function Assert-Markers([bool]$telegram, [bool]$matrix) {
  $k = Get-Item $channelsKey -ErrorAction SilentlyContinue
  $actual = @{ Telegram = ($null -ne $k -and $null -ne $k.GetValue("Telegram")); Matrix = ($null -ne $k -and $null -ne $k.GetValue("Matrix")) }
  if ($actual.Telegram -ne $telegram -or $actual.Matrix -ne $matrix) {
    throw "channel markers: telegram=$($actual.Telegram), expected $telegram; matrix=$($actual.Matrix), expected $matrix"
  }
}

$ver = Msi-Version $Msi
Msiexec "/i `"$Msi`"" "install"
Assert-Installed $ver
Assert-Channels "all"
Assert-Markers $true $true

# The same kit as the next patch versions (updates replace the old install, one entry each).
$cargo = & $Pack version
$core = ($cargo -split '-')[0] -split '\.'
$patch = [int]$core[2]
function Next-Kit([int]$add) {
  $out = Join-Path $env:RUNNER_TEMP "postman-next$add.msi"
  # Pack's stdout ("msi: ...") must not reach the pipeline: it would turn the returned path into an array.
  & $Pack msi $Kit $out --as "$($core[0]).$($core[1]).$($patch + $add)" | Out-Null
  if ($LASTEXITCODE) { throw "postman-pack --as +$add failed" }
  return $out
}

# Update: one entry, new version; the channels are on by default and stay.
$newer = Next-Kit 1
$newVer = Msi-Version $newer
Msiexec "/i `"$newer`"" "update"
Assert-Installed $newVer
Assert-Channels "all"
Assert-Markers $true $true

# CHANNELS sets the exact channel set for the rollout: "none" removes both (PLG-13, decision 2026-11-19).
Msiexec "/i `"$newer`" CHANNELS=none" "channels-none"
Assert-Channels "none"
Assert-Markers $false $false

# The set survives an upgrade: the markers are gone, so the channels stay out.
$newer2 = Next-Kit 2
$newVer2 = Msi-Version $newer2
Msiexec "/i `"$newer2`"" "update2"
Assert-Installed $newVer2
Assert-Channels "none"
Assert-Markers $false $false

# A narrower CHANNELS brings the named channel back, and only it.
Msiexec "/i `"$newer2`" CHANNELS=Telegram" "channels-telegram"
Assert-Channels "telegram"
Assert-Markers $true $false

# REMOVE drops a channel the ordinary way; the next upgrade keeps it out (marker gone).
Msiexec "/i `"$newer2`" REMOVE=Telegram" "remove-telegram"
Assert-Channels "none"
Assert-Markers $false $false

# Downgrade is refused (1603) and leaves the newer version in place.
Msiexec "/i `"$Msi`"" "downgrade" @(1603)
Assert-Installed $newVer2

# The client creates the profiles on the first worker/plugin start; here they are created directly (userenv), as
# the app would.
Add-Type -Namespace Par05 -Name UserEnv -MemberDefinition @'
[DllImport("userenv.dll", CharSet = CharSet.Unicode)]
public static extern int CreateAppContainerProfile(string name, string display, string description, IntPtr caps, uint count, out IntPtr sid);
'@
foreach ($name in $profileNames) {
  $sid = [IntPtr]::Zero
  $hr = [Par05.UserEnv]::CreateAppContainerProfile($name, "Postman", "Postman", [IntPtr]::Zero, 0, [ref]$sid)
  if ($hr -lt 0 -and $hr -ne -2147024713) { throw "CreateAppContainerProfile ${name}: $('0x{0:X8}' -f $hr)" }
  if (-not (Test-Profile $name)) { throw "AppContainer profile $name not created" }
}

Msiexec "/x `"$newer2`"" "remove"
foreach ($name in $profileNames) {
  if (Test-Profile $name) { throw "left after removal: AppContainer profile $name" }
}
if (Test-Path -LiteralPath $dir) { throw "left after removal: $dir" }
if (Test-Path -LiteralPath $lnk) { throw "left after removal: $lnk" }
if (Test-Path $eventKey) { throw "left after removal: $eventKey" }
if (Test-Path $channelsKey) { throw "left after removal: $channelsKey" }
if ((Entries).Count) { throw "left after removal: Installed apps entry" }
"removed: files, shortcut, event source, channel markers, Installed apps entry, AppContainer profiles $($profileNames -join ', ')"
