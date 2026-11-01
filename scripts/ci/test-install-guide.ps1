# Runs the Windows commands of the install guide against a local signed
# release (spec: distribution-and-update, task 20.10): every line of its
# ```powershell blocks, then every `prometheus ...` line of its ```sh blocks
# (shell-agnostic), in order. Like scripts/ci/test-install-guide, the
# release origin is the local fixture (its install.ps1 defaults to it),
# BINARY is published as its own version signed with the committed test key
# (which debug builds trust), and the next patch version is published, with
# `update.mirror_url` pointing at the fixture, before the first
# `prometheus update` command. A command must exit 0, except `prometheus
# update --check`, which exits 10 when it finds the newer release.
#
# usage: scripts/ci/test-install-guide.ps1 -Guide FILE -Binary FILE -Target TRIPLE
# (paths relative to the repository root)
param(
    [Parameter(Mandatory)] [string]$Guide,
    [Parameter(Mandatory)] [string]$Binary,
    [Parameter(Mandatory)] [string]$Target
)
$ErrorActionPreference = 'Stop'

function Fail([string]$Message) {
    [Console]::Error.WriteLine("test-install-guide.ps1: $Message")
    exit 1
}

$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$real = 'https://github.com/CyberdyneCorp/prometheus-agent/releases/latest/download'
$port = 8766
$base = "http://127.0.0.1:$port"
# Relative paths: Git Bash (installer-fixture) and PowerShell read them alike.
Set-Location $root
$fixture = 'target/install-guide/release'

$version = (& $Binary version --json | ConvertFrom-Json).version
$parts = $version.Split('.')
$next = "v$($parts[0]).$($parts[1]).$([int]$parts[2] + 1)"

function Publish([string]$Tag, [string[]]$Extra) {
    & bash scripts/ci/installer-fixture $fixture $Binary $Target $Tag --test-key @Extra
    if ($LASTEXITCODE -ne 0) { Fail "installer-fixture $Tag failed" }
    # The served installer defaults to the fixture, as the real one defaults
    # to the real origin.
    $installer = Join-Path $fixture 'install.ps1'
    (Get-Content -Raw $installer).Replace("[string]`$BaseUrl = 'https://github.com/CyberdyneCorp/prometheus-agent/releases/download'", "[string]`$BaseUrl = '$base'") |
        Set-Content -Encoding UTF8 $installer
}

Publish "v$version" @()
$server = Start-Process python -ArgumentList '-m', 'http.server', "$port", '--bind', '127.0.0.1' `
    -WorkingDirectory $fixture -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2

$dir = Join-Path $env:LOCALAPPDATA 'Programs\Prometheus'
$env:Path = "$dir;$env:Path"
$published = $false
$lines = New-Object System.Collections.Generic.List[string]
$block = ''
foreach ($line in Get-Content $Guide) {
    if ($line -match '^```(\w*)$') {
        $block = if ($block -eq '') { $Matches[1] } else { '' }
        continue
    }
    if ($line.Trim() -eq '') { continue }
    if ($block -eq 'powershell' -or ($block -eq 'sh' -and $line.StartsWith('prometheus '))) {
        $lines.Add($line)
    }
}
if ($lines.Count -eq 0) { Fail "$Guide has no Windows commands" }

try {
    foreach ($line in $lines) {
        $command = $line.Replace($real, $base)
        if ($command.StartsWith('prometheus update') -and -not $published) {
            Publish $next @('--add')
            $daemon = Join-Path $env:LOCALAPPDATA 'Prometheus\daemon.toml'
            Add-Content -Path $daemon -Value "`n[update]`nmirror_url = `"$base`""
            $published = $true
        }
        Write-Output "+ $command"
        # Input is redirected, so the installer never waits on a PATH question.
        # `-Command` reports a failing native command as 1: pass its own code on.
        '' | & pwsh -NoProfile -NonInteractive -Command "$command; if (`$LASTEXITCODE) { exit `$LASTEXITCODE }" | Out-Host
        $code = $LASTEXITCODE
        $allowed = ($code -eq 0) -or ($code -eq 10 -and $command -eq 'prometheus update --check')
        if (-not $allowed) { Fail "``$line`` exited $code" }
    }
} finally {
    Stop-Process -Id $server.Id -Force
}

if (-not $published) { Fail 'the guide runs no update command' }
$excluded = & (Join-Path $dir 'prometheus.exe') runtimes exclude list
if (-not ($excluded -contains 'chromium')) { Fail "-Without chromium was not recorded: $excluded" }
Write-Output 'install guide commands passed (Windows)'
exit 0
