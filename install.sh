#!/usr/bin/env sh
# Install the archeion binary from a GitHub release.
#
#   curl -sSfL https://raw.githubusercontent.com/gabrielassisxyz/archeion/master/install.sh | sh
#
# Env: ARCHEION_VERSION (default: latest), ARCHEION_INSTALL_DIR (default: ~/.local/bin).
# POSIX sh on purpose: the machine being installed to is not guaranteed to have bash.
set -eu

REPO="gabrielassisxyz/archeion"
VERSION="${ARCHEION_VERSION:-latest}"
INSTALL_DIR="${ARCHEION_INSTALL_DIR:-$HOME/.local/bin}"

die() { echo "install: $1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

need curl
need tar
need uname

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)  os_part=linux ;;
    Darwin) os_part=darwin ;;
    *) die "unsupported OS: $os (releases cover Linux and macOS)" ;;
esac
case "$arch" in
    x86_64|amd64)  arch_part=amd64 ;;
    aarch64|arm64) arch_part=arm64 ;;
    *) die "unsupported architecture: $arch" ;;
esac
asset="archeion-${os_part}-${arch_part}.tar.xz"

if [ "$VERSION" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset ($VERSION)"
curl -sSfL "$base/$asset" -o "$tmp/$asset" || die "no such asset: $base/$asset"
curl -sSfL "$base/$asset.sha256" -o "$tmp/$asset.sha256" || die "no checksum published for $asset"

# An unverified download is the whole supply chain of this installer, so a missing
# checksum tool is a failure and never a warning.
echo "Verifying checksum"
( cd "$tmp" && if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -c "$asset.sha256"
  elif command -v shasum >/dev/null 2>&1; then
      shasum -a 256 -c "$asset.sha256"
  else
      die "missing required tool: sha256sum or shasum"
  fi ) >/dev/null || die "checksum mismatch for $asset"

mkdir -p "$INSTALL_DIR"
tar -C "$tmp" -xf "$tmp/$asset"
install -m 0755 "$tmp/archeion" "$INSTALL_DIR/archeion"

echo "Installed $INSTALL_DIR/archeion"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "Note: $INSTALL_DIR is not on PATH." ;;
esac
