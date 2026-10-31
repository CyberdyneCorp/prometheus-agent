#!/bin/sh
# Prometheus installer for macOS, Linux and Termux (spec:
# distribution-and-update, Installers). Rendered by
# `cargo xtask release installers`, which embeds the release public keys.
#
#   curl -fsSL https://github.com/CyberdyneCorp/prometheus-agent/releases/latest/download/install.sh | sh
#   sh install.sh [--channel stable|canary] [--version vX.Y.Z] [--modify-path|--no-modify-path]
#                 [--base-url URL]
#
# It downloads the signed channel index and release manifest, verifies their
# minisign signatures against the embedded keys (with `minisign`, or with
# OpenSSL 3), checks the archive against the signed SHA-256, installs
# ~/.local/bin/prometheus (+ the `pmt` alias) and writes the install receipt
# ~/.local/bin/install.json (owner `self`). It sends nothing but the
# downloads themselves.
set -eu

DEFAULT_BASE_URL="https://github.com/CyberdyneCorp/prometheus-agent/releases/download"
# Minisign public keys (base64) every release file must be signed by.
RELEASE_KEYS="
@@RELEASE_KEYS@@
"

usage() {
    cat <<'USAGE'
usage: install.sh [--channel stable|canary] [--version vX.Y.Z] [--modify-path|--no-modify-path]
                  [--base-url URL]
USAGE
}

die() {
    printf 'install.sh: %s\n' "$1" >&2
    exit "${2:-1}"
}

channel=stable
version=
modify_path=ask
base_url=$DEFAULT_BASE_URL

need_value() {
    [ "$2" -ge 2 ] || die "$1 needs a value" 2
}

while [ $# -gt 0 ]; do
    case $1 in
    --channel) need_value "$1" $#; channel=$2; shift 2 ;;
    --channel=*) channel=${1#*=}; shift ;;
    --version) need_value "$1" $#; version=$2; shift 2 ;;
    --version=*) version=${1#*=}; shift ;;
    --base-url) need_value "$1" $#; base_url=$2; shift 2 ;;
    --base-url=*) base_url=${1#*=}; shift ;;
    --modify-path) modify_path=yes; shift ;;
    --no-modify-path) modify_path=no; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown option $1 (see --help)" 2 ;;
    esac
done

# Arguments are checked before anything is downloaded.
case $channel in
stable | canary) ;;
*) die "--channel must be stable or canary, not $channel" 2 ;;
esac
if [ -n "$version" ] &&
    ! printf '%s\n' "$version" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-canary\.[0-9]{14})?$'; then
    die "--version must be vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-canary.YYYYMMDDHHMMSS, not $version" 2
fi
base_url=${base_url%/}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case $arch in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "unsupported architecture: $arch" ;;
    esac
    case $os in
    Darwin) echo universal2-apple-darwin ;;
    Linux)
        if [ "$(uname -o 2>/dev/null || true)" = Android ]; then
            [ "$arch" = aarch64 ] || die "Termux builds exist for aarch64 only"
            echo aarch64-linux-android
        else
            # Static musl builds run on every Linux distribution.
            echo "$arch-unknown-linux-musl"
        fi
        ;;
    *) die "unsupported OS: $os (on Windows use install.ps1)" ;;
    esac
}

download() { # url file
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "neither curl nor wget is installed"
    fi || die "cannot download $1"
}

hex() { od -An -tx1 | tr -d ' \n'; }

# Ed25519 checks with OpenSSL: minisign signs BLAKE2b-512(file), then
# signature || trusted comment ("global signature").
openssl_verify() { # file sigfile key_b64 -> 0 when it verifies
    work=$tmp/ossl
    rm -rf "$work"
    mkdir "$work"
    printf '%s' "$3" | openssl base64 -d -A >"$work/pk.bin" 2>/dev/null || return 1
    sed -n 2p "$2" | openssl base64 -d -A >"$work/sig.bin" 2>/dev/null || return 1
    sed -n 4p "$2" | openssl base64 -d -A >"$work/global.bin" 2>/dev/null || return 1
    [ "$(dd if="$work/sig.bin" bs=1 count=2 2>/dev/null)" = ED ] || return 1
    [ "$(dd if="$work/sig.bin" bs=1 skip=2 count=8 2>/dev/null | hex)" = \
        "$(dd if="$work/pk.bin" bs=1 skip=2 count=8 2>/dev/null | hex)" ] || return 1
    # SubjectPublicKeyInfo of an Ed25519 key: fixed 12-byte prefix + key.
    {
        printf '\060\052\060\005\006\003\053\145\160\003\041\000'
        dd if="$work/pk.bin" bs=1 skip=10 count=32 2>/dev/null
    } >"$work/pk.der"
    dd if="$work/sig.bin" bs=1 skip=10 count=64 2>/dev/null >"$work/ed.bin"
    openssl dgst -blake2b512 -binary "$1" >"$work/hash.bin" || return 1
    openssl pkeyutl -verify -pubin -inkey "$work/pk.der" -keyform DER -rawin \
        -in "$work/hash.bin" -sigfile "$work/ed.bin" >/dev/null 2>&1 || return 1
    {
        cat "$work/ed.bin"
        sed -n 3p "$2" | sed 's/^trusted comment: //' | tr -d '\n'
    } >"$work/signed-comment.bin"
    openssl pkeyutl -verify -pubin -inkey "$work/pk.der" -keyform DER -rawin \
        -in "$work/signed-comment.bin" -sigfile "$work/global.bin" >/dev/null 2>&1
}

openssl_has_ed25519() {
    command -v openssl >/dev/null 2>&1 &&
        openssl pkeyutl -help 2>&1 | grep -q rawin &&
        openssl dgst -list 2>/dev/null | grep -qi blake2b512
}

verify() { # file name-for-messages: file.minisig must verify with one embedded key
    file=$1
    for key in $RELEASE_KEYS; do
        if command -v minisign >/dev/null 2>&1; then
            minisign -Vqm "$file" -x "$file.minisig" -P "$key" >/dev/null 2>&1 && return 0
        elif openssl_has_ed25519; then
            openssl_verify "$file" "$file.minisig" "$key" && return 0
        else
            die "cannot verify release signatures: install minisign (https://jedisct1.github.io/minisign/) or OpenSSL 3 and run this again"
        fi
    done
    die "signature verification failed for $2" 77
}

fetch_signed() { # path-below-base file
    download "$base_url/$1" "$2"
    download "$base_url/$1.minisig" "$2.minisig"
    verify "$2" "$(basename "$1")"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        openssl dgst -sha256 -r "$1" | cut -d' ' -f1
    fi
}

json_field() { # name < one-line JSON object
    sed -n "s/.*\"$1\": *\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" | head -n 1
}

json_string() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

shell_profile() {
    case ${SHELL:-} in
    */zsh) echo "$HOME/.zshrc" ;;
    */bash) if [ "$(uname -s)" = Darwin ]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi ;;
    */fish) echo "$HOME/.config/fish/config.fish" ;;
    *) echo "$HOME/.profile" ;;
    esac
}

path_line() { # profile
    case $1 in
    *.fish) echo 'fish_add_path "$HOME/.local/bin"' ;;
    *) echo 'export PATH="$HOME/.local/bin:$PATH"' ;;
    esac
}

can_ask() { [ -t 0 ] || { [ -t 1 ] && [ -r /dev/tty ]; }; }

setup_path() { # bin_dir
    case ":$PATH:" in *":$1:"*) return 0 ;; esac
    profile=$(shell_profile)
    line=$(path_line "$profile")
    if [ "$modify_path" = ask ] && can_ask; then
        printf 'Add %s to PATH in %s? [y/N] ' "$1" "$profile"
        read -r answer </dev/tty || answer=
        case $answer in y | Y | yes | YES) modify_path=yes ;; *) modify_path=no ;; esac
    fi
    if [ "$modify_path" = yes ]; then
        mkdir -p "$(dirname "$profile")"
        grep -qsF "$line" "$profile" || printf '\n%s\n' "$line" >>"$profile"
        echo "added $1 to PATH in $profile (open a new shell to use it)"
    else
        echo "$1 is not on your PATH. Add this line to $profile:"
        echo "    $line"
        echo "To use prometheus in this shell now, run:"
        echo "    export PATH=\"$1:\$PATH\""
    fi
}

target=$(detect_target)
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t prometheus-install)
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -z "$version" ]; then
    fetch_signed "channels/$channel.json" "$tmp/index.json"
    version=$(tr -d '\n' <"$tmp/index.json" | json_field version)
    [ -n "$version" ] || die "the $channel channel index names no version"
fi
fetch_signed "$version/release-manifest.json" "$tmp/manifest.json"
line=$(grep "\"target\":\"$target\"" "$tmp/manifest.json" || true)
[ -n "$line" ] || die "release $version has no archive for $target"
name=$(printf '%s\n' "$line" | json_field name)
sha256=$(printf '%s\n' "$line" | json_field sha256)
download "$base_url/$version/$name" "$tmp/$name"
[ "$(sha256_of "$tmp/$name")" = "$sha256" ] || die "signature verification failed for $name" 77

mkdir "$tmp/x"
tar -xzf "$tmp/$name" -C "$tmp/x"
binary=$(find "$tmp/x" -type f -name prometheus | head -n 1)
[ -n "$binary" ] || die "$name holds no prometheus binary"

bin_dir=$HOME/.local/bin
mkdir -p "$bin_dir"
cp "$binary" "$bin_dir/.prometheus.new.$$"
chmod 755 "$bin_dir/.prometheus.new.$$"
mv -f "$bin_dir/.prometheus.new.$$" "$bin_dir/prometheus"
ln -sf prometheus "$bin_dir/pmt"
cat >"$bin_dir/install.json" <<RECEIPT
{
  "owner": "self",
  "channel": "$channel",
  "version": "$version",
  "target": "$target",
  "installed_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "binary": "$(json_string "$bin_dir/prometheus")"
}
RECEIPT

echo "installed prometheus $version ($target) to $bin_dir/prometheus"
setup_path "$bin_dir"
