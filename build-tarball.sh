#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
die() { printf 'Error: %s\n' "$*" >&2; exit 1; }

build=1
case "$#:${1:-}" in
    0:) ;;
    1:--no-build) build=0 ;;
    *) printf 'Usage: bash build-tarball.sh [--no-build]\n' >&2; exit 2 ;;
esac

target=x86_64-unknown-linux-gnu
target_dir=${CARGO_TARGET_DIR:-target}
out="$target_dir/$target"
bin="$out/release/eafrei"

for file in Cargo.toml config/mail.conf config/php.conf config/web.conf public/index.html; do
    [[ -f "$file" && -r "$file" ]] || die "Missing or unreadable: $file (run this script from the Eafrei source tree)."
done
version=$(awk -F"[\"']" '
    /^[[:space:]]*\[/ { pkg = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*(#.*)?$/) }
    pkg && /^[[:space:]]*version[[:space:]]*=/ { print $2; exit }
' Cargo.toml)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || die 'Cannot read [package].version from Cargo.toml.'
name="eafrei-$version-$target"

if (( build )); then
    command -v cargo >/dev/null || die 'Cargo is required to build. Use --no-build only if the release binary already exists.'
    cargo build --release --locked --bin eafrei --target "$target" --target-dir "$target_dir"
fi
[[ -f "$bin" && -r "$bin" ]] || die "Release binary not found: $bin. Run without --no-build to compile it."

stage=$(mktemp -d -- "$out/.eafrei-pack.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
pkg="$stage/$name"
mkdir -p -- "$pkg/config" "$pkg/public" "$pkg/certs"
install -m 0755 -- "$bin" "$pkg/eafrei"
install -m 0644 -- config/mail.conf "$pkg/config/mail.conf"
install -m 0644 -- config/php.conf "$pkg/config/php.conf"
install -m 0644 -- config/web.conf "$pkg/config/web.conf"
install -m 0644 -- public/index.html "$pkg/public/index.html"
if [[ -f public/.htaccess ]]; then
    install -m 0644 -- public/.htaccess "$pkg/public/.htaccess"
fi

tar -czf "$stage/$name.tar.gz" -C "$stage" "$name"
mv -fT -- "$stage/$name.tar.gz" "$out/$name.tar.gz"
printf 'Built tarball: %s/%s.tar.gz\n' "$out" "$name"
