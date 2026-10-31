# Prometheus installer for Windows (spec: distribution-and-update,
# Installers). Rendered by `cargo xtask release installers`, which embeds
# the release public keys.
#
#   irm https://github.com/CyberdyneCorp/prometheus-agent/releases/latest/download/install.ps1 | iex
#   .\install.ps1 [-Channel stable|canary] [-Version vX.Y.Z] [-ModifyPath|-NoModifyPath] [-BaseUrl URL]
#
# It downloads the signed channel index and release manifest, verifies their
# minisign signatures against the embedded keys (Ed25519 and BLAKE2b in the
# C# below: Windows ships no Ed25519 verifier), checks the archive against
# the signed SHA-256, installs %LOCALAPPDATA%\Programs\Prometheus\prometheus.exe
# (+ the pmt.exe alias) and writes the install receipt install.json beside
# it (owner `self`). It sends nothing but the downloads themselves.
param(
    [string]$Channel = 'stable',
    [string]$Version = '',
    [switch]$ModifyPath,
    [switch]$NoModifyPath,
    [string]$BaseUrl = 'https://github.com/CyberdyneCorp/prometheus-agent/releases/download'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Minisign public keys (base64) every release file must be signed by.
$ReleaseKeys = @'
@@RELEASE_KEYS@@
'@ -split "`r?`n" | Where-Object { $_.Trim() -ne '' }

function Fail([string]$Message, [int]$Code = 1) {
    [Console]::Error.WriteLine("install.ps1: $Message")
    exit $Code
}

# Arguments are checked before anything is downloaded.
if ($Channel -ne 'stable' -and $Channel -ne 'canary') {
    Fail "--channel must be stable or canary, not $Channel" 2
}
if ($Version -ne '' -and $Version -notmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-canary\.[0-9]{14})?$') {
    Fail "--version must be vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-canary.YYYYMMDDHHMMSS, not $Version" 2
}
$BaseUrl = $BaseUrl.TrimEnd('/')

# Windows PowerShell 5.1 (.NET Framework) needs System.Numerics for
# BigInteger; PowerShell 7 compiles against every .NET reference assembly
# unless -ReferencedAssemblies narrows the set, so it gets none.
$references = @{}
if ($PSVersionTable.PSEdition -ne 'Core') { $references.ReferencedAssemblies = 'System.Numerics' }
Add-Type @references -TypeDefinition @'
using System;
using System.Numerics;
using System.Security.Cryptography;

public static class PmtMinisign {
    static readonly BigInteger P = BigInteger.Pow(2, 255) - 19;
    static readonly BigInteger L = BigInteger.Pow(2, 252) + BigInteger.Parse("27742317777372353535851937790883648493");
    static readonly BigInteger D = Mod(-121665 * Inv(121666));
    static readonly BigInteger SqrtM1 = BigInteger.ModPow(2, (P - 1) / 4, P);

    static BigInteger Mod(BigInteger x) { BigInteger r = x % P; return r < 0 ? r + P : r; }
    static BigInteger Inv(BigInteger x) { return BigInteger.ModPow(Mod(x), P - 2, P); }

    static BigInteger FromLE(byte[] b, int off, int len) {
        byte[] t = new byte[len + 1];
        Array.Copy(b, off, t, 0, len);
        return new BigInteger(t);
    }

    struct Pt { public BigInteger X, Y, Z, T; }

    static Pt Add(Pt a, Pt b) {
        BigInteger A = Mod((a.Y - a.X) * (b.Y - b.X)), B = Mod((a.Y + a.X) * (b.Y + b.X));
        BigInteger C = Mod(2 * a.T * b.T * D), Dd = Mod(2 * a.Z * b.Z);
        BigInteger E = B - A, F = Dd - C, G = Dd + C, H = B + A;
        Pt r; r.X = Mod(E * F); r.Y = Mod(G * H); r.Z = Mod(F * G); r.T = Mod(E * H);
        return r;
    }

    static Pt Mul(BigInteger s, Pt p) {
        Pt q; q.X = 0; q.Y = 1; q.Z = 1; q.T = 0;
        while (s > 0) { if (!s.IsEven) q = Add(q, p); p = Add(p, p); s >>= 1; }
        return q;
    }

    static bool Same(Pt a, Pt b) {
        return Mod(a.X * b.Z - b.X * a.Z) == 0 && Mod(a.Y * b.Z - b.Y * a.Z) == 0;
    }

    static Pt Point(BigInteger y, int sign) {
        BigInteger y2 = y * y;
        BigInteger x2 = Mod((y2 - 1) * Inv(D * y2 + 1));
        BigInteger x = BigInteger.ModPow(x2, (P + 3) / 8, P);
        if (Mod(x * x - x2) != 0) x = Mod(x * SqrtM1);
        if (Mod(x * x - x2) != 0) throw new ArgumentException("not a curve point");
        if ((int)(x % 2) != sign) x = Mod(P - x);
        Pt r; r.X = x; r.Y = y; r.Z = 1; r.T = Mod(x * y);
        return r;
    }

    static Pt Decode(byte[] bytes, int off) {
        byte[] b = new byte[32];
        Array.Copy(bytes, off, b, 0, 32);
        int sign = b[31] >> 7;
        b[31] &= 0x7f;
        BigInteger y = FromLE(b, 0, 32);
        if (y >= P) throw new ArgumentException("not a curve point");
        return Point(y, sign);
    }

    static bool Ed25519(byte[] pub, byte[] msg, byte[] sig) {
        try {
            Pt a = Decode(pub, 0), r = Decode(sig, 0);
            BigInteger s = FromLE(sig, 32, 32);
            if (s >= L) return false;
            byte[] input = new byte[64 + msg.Length];
            Array.Copy(sig, 0, input, 0, 32);
            Array.Copy(pub, 0, input, 32, 32);
            Array.Copy(msg, 0, input, 64, msg.Length);
            byte[] h;
            using (SHA512 sha = SHA512.Create()) { h = sha.ComputeHash(input); }
            BigInteger k = FromLE(h, 0, 64) % L;
            Pt b = Point(Mod(4 * Inv(5)), 0);
            return Same(Mul(s, b), Add(r, Mul(k, a)));
        } catch (ArgumentException) {
            return false;
        }
    }

    static readonly ulong[] IV = {
        0x6a09e667f3bcc908UL, 0xbb67ae8584caa73bUL, 0x3c6ef372fe94f82bUL, 0xa54ff53a5f1d36f1UL,
        0x510e527fade682d1UL, 0x9b05688c2b3e6c1fUL, 0x1f83d9abfb41bd6bUL, 0x5be0cd19137e2179UL };
    static readonly int[,] Sigma = {
        { 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15 },
        { 14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3 },
        { 11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4 },
        { 7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8 },
        { 9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13 },
        { 2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9 },
        { 12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11 },
        { 13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10 },
        { 6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5 },
        { 10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0 } };

    static ulong Rot(ulong x, int n) { return (x >> n) | (x << (64 - n)); }

    static void G(ulong[] v, int a, int b, int c, int d, ulong x, ulong y) {
        v[a] = v[a] + v[b] + x; v[d] = Rot(v[d] ^ v[a], 32);
        v[c] = v[c] + v[d]; v[b] = Rot(v[b] ^ v[c], 24);
        v[a] = v[a] + v[b] + y; v[d] = Rot(v[d] ^ v[a], 16);
        v[c] = v[c] + v[d]; v[b] = Rot(v[b] ^ v[c], 63);
    }

    static void Compress(ulong[] h, byte[] block, ulong t, bool last) {
        ulong[] m = new ulong[16];
        for (int i = 0; i < 16; i++) m[i] = BitConverter.ToUInt64(block, i * 8);
        ulong[] v = new ulong[16];
        Array.Copy(h, v, 8);
        Array.Copy(IV, 0, v, 8, 8);
        v[12] ^= t;
        if (last) v[14] = ~v[14];
        for (int r = 0; r < 12; r++) {
            int s = r % 10;
            G(v, 0, 4, 8, 12, m[Sigma[s, 0]], m[Sigma[s, 1]]);
            G(v, 1, 5, 9, 13, m[Sigma[s, 2]], m[Sigma[s, 3]]);
            G(v, 2, 6, 10, 14, m[Sigma[s, 4]], m[Sigma[s, 5]]);
            G(v, 3, 7, 11, 15, m[Sigma[s, 6]], m[Sigma[s, 7]]);
            G(v, 0, 5, 10, 15, m[Sigma[s, 8]], m[Sigma[s, 9]]);
            G(v, 1, 6, 11, 12, m[Sigma[s, 10]], m[Sigma[s, 11]]);
            G(v, 2, 7, 8, 13, m[Sigma[s, 12]], m[Sigma[s, 13]]);
            G(v, 3, 4, 9, 14, m[Sigma[s, 14]], m[Sigma[s, 15]]);
        }
        for (int i = 0; i < 8; i++) h[i] ^= v[i] ^ v[i + 8];
    }

    public static byte[] Blake2b512(byte[] data) {
        ulong[] h = (ulong[])IV.Clone();
        h[0] ^= 0x01010040UL;
        byte[] block = new byte[128];
        int off = 0;
        ulong t = 0;
        while (data.Length - off > 128) {
            Array.Copy(data, off, block, 0, 128);
            t += 128;
            Compress(h, block, t, false);
            off += 128;
        }
        Array.Clear(block, 0, 128);
        int rest = data.Length - off;
        Array.Copy(data, off, block, 0, rest);
        t += (ulong)rest;
        Compress(h, block, t, true);
        byte[] output = new byte[64];
        for (int i = 0; i < 8; i++) Array.Copy(BitConverter.GetBytes(h[i]), 0, output, i * 8, 8);
        return output;
    }

    // Verifies a minisign signature file (prehashed "ED" algorithm) of
    // data with a base64 public key: the Ed25519 signature of
    // BLAKE2b-512(data), then the global signature of signature ||
    // trusted comment.
    public static bool Verify(byte[] data, string minisig, string publicKey) {
        try {
            string[] lines = minisig.Replace("\r", "").Split('\n');
            byte[] pk = Convert.FromBase64String(publicKey.Trim());
            byte[] sig = Convert.FromBase64String(lines[1].Trim());
            byte[] global = Convert.FromBase64String(lines[3].Trim());
            if (pk.Length != 42 || sig.Length != 74 || global.Length != 64) return false;
            if (sig[0] != (byte)'E' || sig[1] != (byte)'D') return false;
            for (int i = 2; i < 10; i++) if (sig[i] != pk[i]) return false;
            byte[] key = new byte[32]; Array.Copy(pk, 10, key, 0, 32);
            byte[] ed = new byte[64]; Array.Copy(sig, 10, ed, 0, 64);
            if (!Ed25519(key, Blake2b512(data), ed)) return false;
            const string prefix = "trusted comment: ";
            if (!lines[2].StartsWith(prefix)) return false;
            byte[] comment = System.Text.Encoding.UTF8.GetBytes(lines[2].Substring(prefix.Length));
            byte[] signed = new byte[64 + comment.Length];
            Array.Copy(ed, 0, signed, 0, 64);
            Array.Copy(comment, 0, signed, 64, comment.Length);
            return Ed25519(key, signed, global);
        } catch (FormatException) {
            return false;
        } catch (IndexOutOfRangeException) {
            return false;
        }
    }
}
'@

function Get-File([string]$Path, [string]$OutFile) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$Path" -OutFile $OutFile
    } catch {
        Fail "cannot download $BaseUrl/${Path}: $($_.Exception.Message)"
    }
}

function Get-Signed([string]$Path, [string]$OutFile) {
    Get-File $Path $OutFile
    Get-File "$Path.minisig" "$OutFile.minisig"
    $data = [IO.File]::ReadAllBytes($OutFile)
    $sig = [IO.File]::ReadAllText("$OutFile.minisig")
    foreach ($key in $ReleaseKeys) {
        if ([PmtMinisign]::Verify($data, $sig, $key)) { return }
    }
    Fail "signature verification failed for $(Split-Path -Leaf $OutFile)" 77
}

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($arch) {
    'X64' { $target = 'x86_64-pc-windows-msvc' }
    'Arm64' { $target = 'aarch64-pc-windows-msvc' }
    default { Fail "unsupported architecture: $arch" }
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("prometheus-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    if ($Version -eq '') {
        Get-Signed "channels/$Channel.json" "$tmp\index.json"
        $Version = (Get-Content -Raw "$tmp\index.json" | ConvertFrom-Json).version
    }
    Get-Signed "$Version/release-manifest.json" "$tmp\manifest.json"
    $manifest = Get-Content -Raw "$tmp\manifest.json" | ConvertFrom-Json
    $artifact = $manifest.artifacts | Where-Object { $_.target -eq $target } | Select-Object -First 1
    if ($null -eq $artifact) { Fail "release $Version has no archive for $target" }
    $archive = Join-Path $tmp $artifact.name
    Get-File "$Version/$($artifact.name)" $archive
    if ((Get-FileHash -Algorithm SHA256 $archive).Hash.ToLower() -ne $artifact.sha256) {
        Fail "signature verification failed for $($artifact.name)" 77
    }

    $unpacked = Join-Path $tmp 'x'
    New-Item -ItemType Directory -Path $unpacked | Out-Null
    & "$env:SystemRoot\System32\tar.exe" -xzf $archive -C $unpacked
    if ($LASTEXITCODE -ne 0) { Fail "cannot unpack $($artifact.name)" }
    $binary = Get-ChildItem -Path $unpacked -Recurse -File -Filter 'prometheus.exe' | Select-Object -First 1
    if ($null -eq $binary) { Fail "$($artifact.name) holds no prometheus.exe" }

    $dir = Join-Path $env:LOCALAPPDATA 'Programs\Prometheus'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $exe = Join-Path $dir 'prometheus.exe'
    Copy-Item $binary.FullName "$exe.new" -Force
    Move-Item "$exe.new" $exe -Force
    $alias = Join-Path $dir 'pmt.exe'
    Remove-Item $alias -Force -ErrorAction SilentlyContinue
    try { New-Item -ItemType HardLink -Path $alias -Target $exe | Out-Null } catch { Copy-Item $exe $alias }

    [ordered]@{
        owner = 'self'
        channel = $Channel
        version = $Version
        target = $target
        installed_at = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        binary = $exe
    } | ConvertTo-Json | Set-Content -Encoding ASCII (Join-Path $dir 'install.json')
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Output "installed prometheus $Version ($target) to $exe"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$onPath = ($userPath -split ';') -contains $dir
if (-not $onPath) {
    $add = $ModifyPath.IsPresent
    if (-not $add -and -not $NoModifyPath.IsPresent -and [Environment]::UserInteractive -and -not [Console]::IsInputRedirected) {
        $add = (Read-Host "Add $dir to your user PATH? [y/N]") -match '^(y|yes)$'
    }
    if ($add) {
        [Environment]::SetEnvironmentVariable('Path', "$dir;$userPath", 'User')
        Write-Output "added $dir to your user PATH (open a new terminal to use it)"
    } else {
        Write-Output "$dir is not on your PATH. To add it for new terminals, run:"
        Write-Output "    [Environment]::SetEnvironmentVariable('Path', `"$dir;`" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
        Write-Output "To use prometheus in this terminal now, run:"
        Write-Output "    `$env:Path = `"$dir;`$env:Path`""
    }
}
