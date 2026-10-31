# Runs install.ps1 against a fixture release server and checks the result
# (spec: distribution-and-update, Installers; Install receipt).
#
# usage: scripts/ci/test-install-ps1.ps1 -Installer FILE -BaseUrl URL -Tag vX.Y.Z [-Shell pwsh|powershell]
param(
    [Parameter(Mandatory)] [string]$Installer,
    [Parameter(Mandatory)] [string]$BaseUrl,
    [Parameter(Mandatory)] [string]$Tag,
    [string]$Shell = 'pwsh'
)
$ErrorActionPreference = 'Stop'

function Fail([string]$Message) {
    [Console]::Error.WriteLine("test-install-ps1 ($Shell): $Message")
    exit 1
}

function Install([string[]]$Arguments) {
    # Send the installer's output to the host: a PowerShell function returns every
    # uncaptured pipeline value, so letting it through would turn the exit code
    # into an array and every comparison against it would fail.
    & $Shell -NoProfile -ExecutionPolicy Bypass -File $Installer @Arguments | Out-Host
    return $LASTEXITCODE
}

# An unknown channel exits 2 before downloading anything.
$code = Install @('-Channel', 'main', '-BaseUrl', 'http://127.0.0.1:9')
if ($code -ne 2) { Fail "-Channel main exited $code, expected 2" }

$code = Install @('-BaseUrl', $BaseUrl, '-NoModifyPath')
if ($code -ne 0) { Fail "install exited $code" }
$dir = Join-Path $env:LOCALAPPDATA 'Programs\Prometheus'
& (Join-Path $dir 'prometheus.exe') version
if ($LASTEXITCODE -ne 0) { Fail 'the installed binary does not run' }
& (Join-Path $dir 'pmt.exe') version
if ($LASTEXITCODE -ne 0) { Fail 'the pmt alias does not run' }
$receipt = Get-Content -Raw (Join-Path $dir 'install.json') | ConvertFrom-Json
if ($receipt.owner -ne 'self') { Fail "receipt owner is $($receipt.owner)" }
if ($receipt.channel -ne 'stable') { Fail "receipt channel is $($receipt.channel)" }
if ($receipt.version -ne $Tag) { Fail "receipt version is $($receipt.version)" }
$status = & (Join-Path $dir 'prometheus.exe') --home (Join-Path $env:TEMP 'pmt-installer-home') update --status
if (-not ($status -contains 'owner: self')) { Fail "update --status does not report owner self: $status" }

# A tampered archive is refused and leaves the installed binary alone.
$before = (Get-FileHash (Join-Path $dir 'prometheus.exe')).Hash
$code = Install @('-BaseUrl', "$BaseUrl/tampered", '-NoModifyPath')
if ($code -ne 77) { Fail "tampered release exited $code, expected 77" }
if ((Get-FileHash (Join-Path $dir 'prometheus.exe')).Hash -ne $before) { Fail 'tampered release changed the binary' }
Write-Output "install.ps1 checks passed ($Shell)"
# Exit explicitly: otherwise the caller sees $LASTEXITCODE from the last
# native command, which is the tampered install's deliberate 77.
exit 0
