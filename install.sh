#!/bin/sh
# Installs quadra on macOS and Linux:
#
#   curl -fsSL https://quadra.run/install.sh | sh
#
# It downloads the official binary for this machine from the releases of
# github.com/adileo/quadra, checks its SHA-256, puts it in ~/.quadra/bin and adds that folder to
# the PATH of your shell (zsh, bash, fish or ~/.profile). Run it again to reinstall.
#
#   QUADRA_VERSION=0.5.0       a version instead of the latest
#   QUADRA_INSTALL=~/somewhere the folder (default ~/.quadra; the binary goes in its bin/)
#   QUADRA_NO_MODIFY_PATH=1    leave the shell's configuration alone
set -eu

releases="${QUADRA_RELEASES_URL:-https://github.com/adileo/quadra/releases}"
home_dir="${QUADRA_INSTALL:-$HOME/.quadra}"
bin_dir="$home_dir/bin"

say() { printf '%s\n' "$*" >&2; }
fail() { say "error: $*"; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is needed"; }
need uname
need gunzip
# HTTPS only, unless QUADRA_RELEASES_URL points elsewhere on purpose (a test, a mirror)
proto="--proto =https --tlsv1.2"; wget_https="--https-only"
if [ -n "${QUADRA_RELEASES_URL:-}" ]; then proto=""; wget_https=""; fi
if command -v curl >/dev/null 2>&1; then
  # shellcheck disable=SC2086
  fetch() { curl -fsSL $proto -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  # shellcheck disable=SC2086
  fetch() { wget -q $wget_https -O "$2" "$1"; }
else
  fail "curl or wget is needed"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "sha256sum or shasum is needed"
fi

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) fail "no official binary for the processor $arch (build from the sources: https://github.com/adileo/quadra)" ;;
esac
case "$os" in
  Darwin)
    # an x86_64 shell under Rosetta on an Apple chip: the native binary is better
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then arch=aarch64; fi
    target="$arch-apple-darwin" ;;
  Linux) target="$arch-unknown-linux-musl" ;;
  *) fail "no official binary for $os: on Windows use install.ps1 (see https://quadra.run/guide/installation)" ;;
esac

if [ -n "${QUADRA_VERSION:-}" ]; then
  base="$releases/download/v${QUADRA_VERSION#v}"
else
  base="$releases/latest/download"
fi
asset="quadra-$target.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
say "Downloading quadra ($target)…"
fetch "$base/$asset" "$tmp/$asset" || fail "cannot download $base/$asset"
fetch "$base/$asset.sha256" "$tmp/$asset.sha256" || fail "cannot download $base/$asset.sha256"
want=$(cut -d' ' -f1 < "$tmp/$asset.sha256")
got=$(sha256 "$tmp/$asset")
[ "$want" = "$got" ] || fail "the download does not match its SHA-256 ($got, expected $want): nothing was installed"
gunzip -c "$tmp/$asset" > "$tmp/quadra"
chmod 755 "$tmp/quadra"
mkdir -p "$bin_dir"
mv -f "$tmp/quadra" "$bin_dir/quadra"
version=$("$bin_dir/quadra" --version 2>/dev/null || echo quadra)
say "Installed $version in $bin_dir/quadra"

# the PATH: the line is added once, to the configuration of the shell in use
case ":$PATH:" in
  *":$bin_dir:"*) on_path=1 ;;
  *) on_path=0 ;;
esac
if [ "$on_path" = 1 ] || [ -n "${QUADRA_NO_MODIFY_PATH:-}" ]; then
  [ "$on_path" = 1 ] || say "Add $bin_dir to your PATH to run quadra."
  say "Next: quadra init my-app"
  exit 0
fi
line="export PATH=\"$bin_dir:\$PATH\""
add() {
  file="$1"; text="$2"
  if [ -f "$file" ] && grep -qF "$bin_dir" "$file"; then return 0; fi
  mkdir -p "$(dirname "$file")"
  printf '\n# quadra\n%s\n' "$text" >> "$file"
  say "Added $bin_dir to the PATH in $file"
}
case "$(basename "${SHELL:-sh}")" in
  zsh) add "${ZDOTDIR:-$HOME}/.zshrc" "$line" ;;
  bash)
    add "$HOME/.bashrc" "$line"
    # login shells (the macOS Terminal) read .bash_profile and not .bashrc
    if [ "$os" = Darwin ] || [ -f "$HOME/.bash_profile" ]; then add "$HOME/.bash_profile" "$line"; fi ;;
  fish) add "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/quadra.fish" "fish_add_path $bin_dir" ;;
  *) add "$HOME/.profile" "$line" ;;
esac
say ""
say "Open a new terminal (or run: export PATH=\"$bin_dir:\$PATH\"), then: quadra init my-app"
