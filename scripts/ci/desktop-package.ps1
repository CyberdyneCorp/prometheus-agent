# Builds and checks the Windows desktop packages of a release (spec:
# distribution-and-update, Desktop packages and signing). Run by the release
# workflow's Windows desktop jobs:
#
#   desktop-package.ps1 -Step app -Dist <dist> -Out <dir>
#       on each architecture's runner: the desktop app executable, the
#       release's CLI and its install receipts (msix, msstore)
#   desktop-package.ps1 -Step bundle -Apps <dir> -Manifests <dir> -Out <dir>
#       the MSIX packages (x64, arm64) of the App Installer identity bundled
#       into the .msixbundle, Authenticode-signed (a self-signed certificate
#       in the dry run), installed here to check the `prometheus.exe` and
#       `pmt.exe` execution aliases; and the Microsoft Store bundle (its own
#       identity, signed by the Store on submission)
#
# Environment: PLAN, PUBLISHING; on a tag PMT_WINDOWS_CERT_PFX (base64),
# PMT_WINDOWS_CERT_PASSWORD. The publisher in the manifests is the
# certificate's subject. Only checked packages reach -Out, so a failed
# signing step publishes no Windows package.
param(
    [Parameter(Mandatory)] [ValidateSet("app", "bundle")] [string] $Step,
    [string] $Dist,
    [string] $Apps,
    [string] $Manifests,
    [Parameter(Mandatory)] [string] $Out
)
$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true

$Root = Resolve-Path (Join-Path $PSScriptRoot "../..")
$Tag = (ConvertFrom-Json $env:PLAN).announcement_tag
$Version = $Tag.TrimStart("v")
$Channel = if ($Tag -like "*-canary.*") { "canary" } else { "stable" }
$env:PMT_DESKTOP_CHANNEL = $Channel
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = Resolve-Path $Out

function Fail([string] $Message) { throw "desktop-package: $Message" }

# The newest Windows SDK tool for this machine's architecture.
function SdkTool([string] $Name) {
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
    $tool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\$arch\$Name" |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $tool) { Fail "the Windows SDK has no $Name" }
    $tool.FullName
}

function App {
    $triple = (rustc -vV | Select-String "^host: (.*)").Matches[0].Groups[1].Value
    $arch = if ($triple -like "aarch64-*") { "arm64" } else { "x64" }
    $unpacked = Join-Path $env:RUNNER_TEMP "cli"
    New-Item -ItemType Directory -Force -Path $unpacked | Out-Null
    tar -xzf (Join-Path $Dist "prometheus-$triple.tar.gz") -C $unpacked
    $cli = Get-ChildItem $unpacked -Recurse -Filter prometheus.exe | Select-Object -First 1
    $dir = Join-Path $Out $arch
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $config = Get-Content (Join-Path $Root "apps/desktop/src-tauri/tauri.release.conf.json") -Raw |
        ConvertFrom-Json
    $config | Add-Member -NotePropertyName version -NotePropertyValue $Version -Force
    $configFile = Join-Path $env:RUNNER_TEMP "tauri.json"
    $config | ConvertTo-Json -Depth 10 | Set-Content $configFile
    Push-Location (Join-Path $Root "apps/desktop/src-tauri")
    cargo tauri build --ci --no-bundle --config $configFile
    Pop-Location
    Copy-Item (Join-Path $Root "target/desktop/release/prometheus-desktop.exe") $dir
    # Each identity's receipt, stamped by this architecture's CLI.
    foreach ($owner in "msix", "msstore") {
        $stamp = Join-Path $env:RUNNER_TEMP "stamp-$owner"
        New-Item -ItemType Directory -Force -Path $stamp | Out-Null
        Copy-Item $cli.FullName $stamp
        & (Join-Path $stamp "prometheus.exe") install-receipt --owner $owner --channel $Channel | Out-Null
        Move-Item (Join-Path $stamp "install.json") (Join-Path $dir "install-$owner.json")
    }
    Copy-Item $cli.FullName $dir
}

# Packs one architecture's MSIX of one identity.
function Pack([string] $Arch, [string] $Identity, [string] $Owner, [string] $Into) {
    $layout = Join-Path $env:RUNNER_TEMP "layout-$Identity-$Arch"
    $assets = Join-Path $layout "Assets"
    New-Item -ItemType Directory -Force -Path $assets | Out-Null
    $files = Join-Path $Apps $Arch
    Copy-Item (Join-Path $files "prometheus-desktop.exe"), (Join-Path $files "prometheus.exe") $layout
    Copy-Item (Join-Path $files "install-$Owner.json") (Join-Path $layout "install.json")
    $icon = Join-Path $Root "apps/desktop/src-tauri/icons/icon.png"
    foreach ($logo in "Square44x44Logo", "Square150x150Logo", "StoreLogo") {
        Copy-Item $icon (Join-Path $assets "$logo.png")
    }
    Copy-Item (Join-Path $Manifests "AppxManifest-$Identity-$Arch.xml") (Join-Path $layout "AppxManifest.xml")
    & (SdkTool makeappx.exe) pack /o /d $layout /p (Join-Path $Into "prometheus-desktop-$Arch.msix")
}

# The code-signing certificate: the release's on a tag, else a self-signed
# one this machine trusts (the dry run).
function Certificate {
    $pfx = Join-Path $env:RUNNER_TEMP "signing.pfx"
    if ($env:PUBLISHING -eq "true") {
        if (-not $env:PMT_WINDOWS_CERT_PFX) { Fail "PMT_WINDOWS_CERT_PFX is not set" }
        [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:PMT_WINDOWS_CERT_PFX))
        return @{ File = $pfx; Password = $env:PMT_WINDOWS_CERT_PASSWORD }
    }
    $publisher = (Get-Content (Join-Path $Manifests "publisher.txt") -Raw).Trim()
    $cert = New-SelfSignedCertificate -Type Custom -Subject $publisher -KeyUsage DigitalSignature `
        -CertStoreLocation Cert:\CurrentUser\My -FriendlyName "Prometheus dry run" `
        -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")
    $password = [Guid]::NewGuid().ToString()
    Export-PfxCertificate -Cert $cert -FilePath $pfx -Password (ConvertTo-SecureString $password -AsPlainText -Force) | Out-Null
    $cer = Join-Path $env:RUNNER_TEMP "signing.cer"
    Export-Certificate -Cert $cert -FilePath $cer | Out-Null
    Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
    Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null
    @{ File = $pfx; Password = $password }
}

# Installs the signed bundle and checks the execution aliases run the
# bundled CLI: the release's version, owned by App Installer.
function CheckInstalled([string] $Bundle) {
    Add-AppxPackage -Path $Bundle
    $aliases = Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps"
    $env:PROMETHEUS_HOME = Join-Path $env:RUNNER_TEMP "home"
    foreach ($alias in "prometheus.exe", "pmt.exe") {
        $reported = (& (Join-Path $aliases $alias) --json version | ConvertFrom-Json).version
        if ($reported -ne $Version) { Fail "$alias reports $reported, not $Version" }
    }
    $PSNativeCommandUseErrorActionPreference = $false
    $output = & (Join-Path $aliases "prometheus.exe") update 2>&1 | Out-String
    $code = $LASTEXITCODE
    $PSNativeCommandUseErrorActionPreference = $true
    if ($code -ne 69 -or $output -notmatch "owner: msix") {
        Fail "prometheus update on the MSIX install exited ${code}: $output"
    }
    Get-AppxPackage -Name "CyberdyneCorp.Prometheus" | Remove-AppxPackage
}

function Bundle {
    $bundleVersion = (Get-Content (Join-Path $Manifests "version.txt") -Raw).Trim()
    $makeappx = SdkTool makeappx.exe
    $signtool = SdkTool signtool.exe
    $direct = Join-Path $env:RUNNER_TEMP "msix-direct"
    $store = Join-Path $env:RUNNER_TEMP "msix-store"
    New-Item -ItemType Directory -Force -Path $direct, $store | Out-Null
    foreach ($arch in "x64", "arm64") {
        Pack $arch "direct" "msix" $direct
        Pack $arch "store" "msstore" $store
    }
    $bundle = Join-Path $Out "prometheus-desktop-x86_64-pc-windows-msvc.msixbundle"
    & $makeappx bundle /o /d $direct /bv $bundleVersion /p $bundle
    $cert = Certificate
    & $signtool sign /fd SHA256 /f $cert.File /p $cert.Password /tr http://timestamp.digicert.com /td SHA256 $bundle
    & $signtool verify /pa /v $bundle
    CheckInstalled $bundle
    Copy-Item (Join-Path $Manifests "prometheus-desktop.appinstaller") $Out
    # The Store signs what it publishes; this bundle goes to the submission.
    $storeOut = Join-Path $Out "store"
    New-Item -ItemType Directory -Force -Path $storeOut | Out-Null
    & $makeappx bundle /o /d $store /bv $bundleVersion /p (Join-Path $storeOut "prometheus-desktop-store.msixbundle")
}

switch ($Step) {
    "app" { App }
    "bundle" { Bundle }
}
