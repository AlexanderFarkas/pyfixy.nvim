#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$ROOT/bin"
BIN="$BIN_DIR/pyfixy-lsp"
REPO="${PYFIXY_REPO:-}"
VERSION="${PYFIXY_VERSION:-latest}"

mkdir -p "$BIN_DIR"

if [[ -z "$REPO" ]]; then
  remote_url="$(git -C "$ROOT" config --get remote.origin.url 2>/dev/null || true)"
  case "$remote_url" in
    git@github.com:*) REPO="${remote_url#git@github.com:}"; REPO="${REPO%.git}" ;;
    https://github.com/*) REPO="${remote_url#https://github.com/}"; REPO="${REPO%.git}" ;;
  esac
fi

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) os="" ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64) arch="x86_64" ;;
  *) arch="" ;;
esac

download_release() {
  [[ -n "$REPO" && -n "$os" && -n "$arch" ]] || return 1
  local target="${arch}-${os}"
  local asset="pyfixy-lsp-${target}.tar.gz"
  local url
  if [[ "$VERSION" == "latest" ]]; then
    url="https://github.com/${REPO}/releases/latest/download/${asset}"
  else
    url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
  fi
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  if command -v curl >/dev/null 2>&1; then
    curl -fL "$url" -o "$tmp/$asset" || return 1
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp/$asset" "$url" || return 1
  else
    return 1
  fi
  tar -xzf "$tmp/$asset" -C "$tmp"
  install -m 0755 "$tmp/pyfixy-lsp" "$BIN"
}

build_from_source() {
  command -v cargo >/dev/null 2>&1 || {
    echo "pyfixy.nvim: no release binary found and cargo is not installed" >&2
    exit 1
  }
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
  install -m 0755 "$ROOT/target/release/pyfixy-lsp" "$BIN"
}

if download_release; then
  echo "pyfixy.nvim: installed $BIN from GitHub release"
else
  echo "pyfixy.nvim: release binary unavailable; building from source"
  build_from_source
fi
