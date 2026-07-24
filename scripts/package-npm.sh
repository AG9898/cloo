#!/usr/bin/env sh
# Package one native cloo binary as the npm optional dependency for its target.
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 3 ]; then
  echo "usage: scripts/package-npm.sh <rust-target> [binary] [output-dir]" >&2
  exit 64
fi

target="$1"
binary="${2:-target/$target/release/cloo}"
output_dir="${3:-dist/npm}"

case "$target" in
  aarch64-apple-darwin) package_key="darwin-arm64" ;;
  x86_64-apple-darwin) package_key="darwin-x64" ;;
  aarch64-unknown-linux-gnu) package_key="linux-arm64" ;;
  x86_64-unknown-linux-gnu) package_key="linux-x64" ;;
  *)
    echo "unsupported release target: $target" >&2
    exit 64
    ;;
esac

if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
  echo "missing executable for $target: $binary" >&2
  exit 1
fi

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
case "$output_dir" in
  /*) ;;
  *) output_dir="$repo_root/$output_dir" ;;
esac

source_dir="$repo_root/npm/packages/$package_key"
stage_dir="$output_dir/clooterminal-$package_key"
root_version=$(node -e 'console.log(require(process.argv[1]).version)' "$repo_root/npm/package.json")
package_version=$(node -e 'console.log(require(process.argv[1]).version)' "$source_dir/package.json")
if [ "$root_version" != "$package_version" ]; then
  echo "version mismatch: clooterminal is $root_version but $package_key is $package_version" >&2
  exit 1
fi

mkdir -p "$output_dir"
rm -rf "$stage_dir"
mkdir -p "$stage_dir/bin"
cp "$source_dir/package.json" "$stage_dir/package.json"
cp "$repo_root/npm/README.md" "$stage_dir/README.md"
cp "$binary" "$stage_dir/bin/cloo"
chmod 755 "$stage_dir/bin/cloo"

(cd "$stage_dir" && npm pack --ignore-scripts --pack-destination "$output_dir")
