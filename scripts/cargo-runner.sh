#!/bin/sh
# Cargo runner on macOS (.cargo/config.toml): runs the app from a signed app
# bundle (scripts/bundle.sh). The bundle lets macOS show notifications, and
# signing with an Apple Development certificate keeps Keychain access
# granted once across rebuilds (see docs/signing.md).
#
# Test binaries (target/*/deps/*) and other programs run unchanged.
BIN="$1"
case "$BIN" in
    */deps/*) ;;
    */ziggy)
        if [ -z "${SIGNAL_GPUI_SIGN_IDENTITY:-}" ] \
            && ! security find-identity -v -p codesigning 2>/dev/null | grep -q '"Apple Development: '; then
            echo "note: no Apple Development certificate, so macOS asks for Keychain access after each rebuild." >&2
            echo "      docs/signing.md explains how to stop that." >&2
        fi
        if BUNDLED=$("$(dirname "$0")/bundle.sh" "$BIN"); then
            shift
            exec "$BUNDLED" "$@"
        fi
        ;;
esac
exec "$@"
