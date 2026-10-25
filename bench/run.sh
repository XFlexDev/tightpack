#!/usr/bin/env bash
# Compare tightpack with common archivers on a folder: ./bench/run.sh <folder>
set -euo pipefail
src=$(realpath "$1")
bin=${TIGHTPACK:-$(dirname "$0")/../target/release/tightpack}
bin=$(realpath "$bin")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
name=$(basename "$src")
parent=$(dirname "$src")
orig=$(du -sb --apparent-size "$src" | cut -f1)

row() { # label size seconds
  printf "| %-26s | %12s | %6.1f%% | %7.1fs |\n" "$1" "$2" "$(echo "$2*100/$orig" | bc -l)" "$3"
}
timeit() { local s=$(date +%s.%N); "$@" >/dev/null 2>&1; echo "$(date +%s.%N) - $s" | bc -l; }

echo "| tool | bytes | of original | time |"
echo "|---|---:|---:|---:|"
row "original" "$orig" 0
cd "$parent"
t=$(timeit zip -qr -9 "$work/a.zip" "$name"); row "zip -9" "$(stat -c%s "$work/a.zip")" "$t"
t=$(timeit rar a -m5 -s -md1g -idq "$work/a.rar" "$name"); row "rar -m5 solid" "$(stat -c%s "$work/a.rar")" "$t"
t=$(timeit 7z a -mx9 "$work/a.7z" "$name"); row "7z -mx9" "$(stat -c%s "$work/a.7z")" "$t"
t=$(timeit sh -c "tar cf - '$name' | zstd -19 --long=27 -T0 -q -o '$work/a.tar.zst'"); row "tar + zstd -19 --long" "$(stat -c%s "$work/a.tar.zst")" "$t"
t=$(timeit sh -c "tar cf - '$name' | xz -9e -T0 > '$work/a.tar.xz'"); row "tar + xz -9e" "$(stat -c%s "$work/a.tar.xz")" "$t"
t=$(timeit "$bin" pack "$name" -o "$work/a.tpak"); row "tightpack" "$(stat -c%s "$work/a.tpak")" "$t"
t=$(timeit "$bin" pack --max "$name" -o "$work/b.tpak"); row "tightpack --max" "$(stat -c%s "$work/b.tpak")" "$t"
t=$(timeit "$bin" unpack "$work/a.tpak" -C "$work/out"); echo "tightpack unpack: ${t}s"
diff -r --no-dereference "$src" "$work/out/$name" && echo "round trip: identical"
