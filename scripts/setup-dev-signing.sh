#!/bin/sh
# Gives development builds a stable code signature.
#
# The database key lives in the login keychain, which checks two things
# before letting an app read it silently: its access list (an app's
# designated requirement, which this self-signed "signal-gpui dev"
# certificate keeps stable across builds) and its partition list. The
# partition list names apps by team ID, and an app without one, like any
# self-signed build, by its build hash, which changes every build. So with
# this certificate alone macOS still asks once per new build.
#
# To stop that, get a free "Apple Development" certificate (Xcode → Settings
# → Accounts → your Apple ID → Manage Certificates → + → Apple Development).
# scripts/bundle.sh uses it automatically; after one more "Always Allow",
# builds stop asking. docs/signing.md has the details.
#
# macOS asks for your password once, to trust the certificate. To undo:
# delete "signal-gpui dev" in Keychain Access (My Certificates).
set -eu

NAME="signal-gpui dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning | grep -q "\"$NAME\""; then
    echo "\"$NAME\" is already set up."
    exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$NAME" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" 2>/dev/null

# macOS can't read OpenSSL 3's default PKCS#12 encryption.
LEGACY=""
if openssl version | grep -q '^OpenSSL 3'; then
    LEGACY="-legacy"
fi
openssl pkcs12 -export $LEGACY -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
    -out "$TMP/identity.p12" -passout pass:signal-gpui

# -T lets codesign use the key without a prompt of its own.
security import "$TMP/identity.p12" -k "$KEYCHAIN" -P signal-gpui -T /usr/bin/codesign

echo "macOS will ask for your password to trust \"$NAME\" for code signing."
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$TMP/cert.pem"

security find-identity -v -p codesigning | grep "\"$NAME\""
echo "Done. The next 'cargo run' asks for Keychain access once more; choose Always Allow."
