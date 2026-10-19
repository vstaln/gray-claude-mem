#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-claude-mem.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-claude-mem" ;;
    *) rel="x86_64-apple-darwin/gray-claude-mem" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-claude-mem" ;;
    *) rel="x86_64-unknown-linux-musl/gray-claude-mem" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-claude-mem.exe" "$here/target/release/gray-claude-mem" "$here/target/debug/gray-claude-mem.exe" "$here/target/debug/gray-claude-mem"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-claude-mem binary not found in $here" >&2
exit 1
