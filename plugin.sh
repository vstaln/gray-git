#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/git.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/git" ;;
    *) rel="x86_64-apple-darwin/git" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/git" ;;
    *) rel="x86_64-unknown-linux-musl/git" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/git.exe" "$here/target/release/git" "$here/target/debug/git.exe" "$here/target/debug/git"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "git binary not found in $here" >&2
exit 1
