#!/bin/sh
# Builds orbitd, stages it into the image tree and runs mkosi. Usage: ./build.sh [mkosi args, e.g. -f]
# The image is Debian trixie (glibc 2.41): build on a host with that glibc or older, or use the Docker build stage
# (`docker build --target build`) and copy /out/orbitd to mkosi.extra/usr/bin/ yourself.
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$here/../..
if [ -z "${ORBITD_BIN:-}" ]; then
    (cd "$root" && cargo build --release --locked -p orbitd)
    ORBITD_BIN=$root/target/release/orbitd
fi
install -D -m 0755 "$ORBITD_BIN" "$here/mkosi.extra/usr/bin/orbitd"
cd "$here"
exec mkosi "$@" build
