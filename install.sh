#!/bin/sh
# Install the auditroo binary for this computer.
# Usage: curl -fsSL https://raw.githubusercontent.com/vincentsch/auditroo/main/install.sh | sh
#
# AUDITROO_VERSION   release tag to install, for example v0.1.0 (default: latest)
# INSTALL_DIR        where to put the binary (default: $HOME/.local/bin)
set -eu

fail() { printf '%s\n' "auditroo install: $*" >&2; exit 1; }
work=
staged=
cleanup() {
    [ -z "$staged" ] || rm -f "$staged"
    [ -z "$work" ] || rm -rf "$work"
    return 0
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

os=$(uname -s)
case "$os" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    MINGW*|MSYS*|CYGWIN*) os=windows ;;
    *) fail "unsupported operating system: $os" ;;
esac
arch=$(uname -m)
case "$arch" in
    x86_64|amd64|AMD64) arch=amd64 ;;
    aarch64|arm64|ARM64) arch=arm64 ;;
    *) fail "unsupported architecture: $arch (auditroo has builds for amd64 and arm64)" ;;
esac
asset=auditroo-$os-$arch
binary=auditroo
if [ "$os" = windows ]; then asset=$asset.exe; binary=$binary.exe; fi
base=${AUDITROO_DOWNLOAD_BASE:-https://github.com/vincentsch/auditroo}
if [ -n "${AUDITROO_VERSION:-}" ]; then
    url=${base%/}/releases/download/$AUDITROO_VERSION
else
    url=${base%/}/releases/latest/download
fi
command -v curl >/dev/null 2>&1 || fail 'curl is needed to download the release'
work=$(mktemp -d) || fail 'cannot create a temporary folder'
curl -fsSL "$url/$asset" -o "$work/$asset" || fail "download failed: $url/$asset"
curl -fsSL "$url/checksums.txt" -o "$work/checksums.txt" || fail 'checksums.txt download failed'
expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name {print $1}' "$work/checksums.txt")
[ -n "$expected" ] || fail "checksums.txt has no entry for $asset"
[ "${#expected}" -eq 64 ] || fail "checksums.txt has a broken entry for $asset"
case "$expected" in *[!0-9a-fA-F]*) fail "checksums.txt has a broken entry for $asset" ;; esac
if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum "$work/$asset") || fail 'SHA-256 calculation failed'
elif command -v shasum >/dev/null 2>&1; then
    digest=$(shasum -a 256 "$work/$asset") || fail 'SHA-256 calculation failed'
else
    fail 'sha256sum or shasum is needed to check the download'
fi
actual=${digest%% *}
[ "$actual" = "$expected" ] || fail "the checksum of $asset does not match. The download may be damaged, so try again"

install_dir=${INSTALL_DIR:-$HOME/.local/bin}
mkdir -p "$install_dir" || fail "cannot create $install_dir; set INSTALL_DIR to a folder you can write to"
# Stage next to the target so the final step is a rename on one file system.
staged=$(mktemp "$install_dir/.auditroo.XXXXXX") || fail "cannot write to $install_dir"
cp "$work/$asset" "$staged" || fail 'cannot copy the binary'
chmod 755 "$staged" || fail 'cannot make the binary executable'
mv -f "$staged" "$install_dir/$binary" || fail "cannot replace $install_dir/$binary"
staged=
printf 'Installed %s to %s.\n' "$binary" "$install_dir"
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) printf 'Add the folder to your PATH first:\n  export PATH="%s:$PATH"\n' "$install_dir" ;;
esac
printf 'Try it: auditroo demo\n'
