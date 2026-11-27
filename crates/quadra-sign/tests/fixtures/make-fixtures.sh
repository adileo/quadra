#!/bin/sh
# Regenerates the test keys and certificates in this directory.
# They are throwaway test material: never use them for anything real.
#
# Made with OpenSSL 4.1.0 and GnuPG 2.5.21:
#   cd crates/quadra-sign/tests/fixtures && sh make-fixtures.sh
#
# Validity windows are fixed so that the tests (which sign at 2026-06-01T12:00:00Z)
# keep working; the leaf certificates expire on 2030-12-31, which the expiry tests use.
set -eu
OPENSSL=${OPENSSL:-/opt/homebrew/bin/openssl}
GPG=${GPG:-/opt/homebrew/bin/gpg}

# Certificate authority: RSA 2048, self-signed.
$OPENSSL genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out ca.key
$OPENSSL req -x509 -new -key ca.key -sha256 -subj "/C=IT/O=Quadra Test/CN=Quadra Test CA" \
  -not_before 20250101000000Z -not_after 20450101000000Z \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -out ca.pem

cat > leaf.ext <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,nonRepudiation
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid
EOF

# RSA 2048 leaf: PKCS#8 and PKCS#1 forms of the same key.
$OPENSSL genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out rsa.key
$OPENSSL rsa -in rsa.key -traditional -out rsa-pkcs1.key
$OPENSSL req -new -key rsa.key -subj "/C=IT/O=ACME S.p.A./CN=ACME S.p.A." -out rsa.csr
$OPENSSL x509 -req -in rsa.csr -CA ca.pem -CAkey ca.key -set_serial 0x1001 -sha256 \
  -not_before 20250101000000Z -not_after 20301231235959Z -extfile leaf.ext -out rsa.pem

# EC P-256 leaf: PKCS#8 and SEC1 forms of the same key.
$OPENSSL genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ec.key
$OPENSSL ec -in ec.key -out ec-sec1.key
$OPENSSL req -new -key ec.key -subj "/C=IT/O=Beta S.r.l./CN=Beta S.r.l." -out ec.csr
$OPENSSL x509 -req -in ec.csr -CA ca.pem -CAkey ca.key -set_serial 0x1002 -sha256 \
  -not_before 20250101000000Z -not_after 20301231235959Z -extfile leaf.ext -out ec.pem

# PKCS#12 with the OpenSSL 3+/4 defaults (AES-256-CBC, PBKDF2, HMAC-SHA256 MAC).
$OPENSSL pkcs12 -export -inkey rsa.key -in rsa.pem -certfile ca.pem -passout pass:quadra-test -out rsa.p12
$OPENSSL pkcs12 -export -inkey ec.key -in ec.pem -certfile ca.pem -passout pass:quadra-test -out ec.p12
# PBMAC1 (RFC 9579) instead of the classic PKCS#12 MAC.
$OPENSSL pkcs12 -export -pbmac1_pbkdf2 -inkey ec.key -in ec.pem -certfile ca.pem -passout pass:quadra-test -out ec-pbmac1.p12
# The legacy format (3DES for the key, RC2-40 for the certificates, SHA-1 MAC).
$OPENSSL pkcs12 -export -legacy -inkey rsa.key -in rsa.pem -certfile ca.pem -passout pass:quadra-test -out rsa-legacy.p12
# 3DES for both, SHA-1 MAC (the format of many older smart-card and CA exports).
$OPENSSL pkcs12 -export -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -inkey rsa.key -in rsa.pem -certfile ca.pem -passout pass:quadra-test -out rsa-3des.p12

rm -f rsa.csr ec.csr leaf.ext

# OpenPGP keys, v4: a signer (RSA 2048, passphrase "pgp-pass") and a recipient
# (Ed25519 primary key with a Curve25519 encryption subkey, no passphrase).
HOME_DIR=$(mktemp -d)
$GPG --homedir "$HOME_DIR" --batch --pinentry-mode loopback --passphrase pgp-pass \
  --quick-gen-key "Quadra Signer <signer@example.com>" rsa2048 sign,cert never
SIGNER=$($GPG --homedir "$HOME_DIR" --list-keys --with-colons signer@example.com | awk -F: '/^fpr/ {print $10; exit}')
$GPG --homedir "$HOME_DIR" --batch --pinentry-mode loopback --passphrase pgp-pass \
  --quick-add-key "$SIGNER" rsa2048 encr never
$GPG --homedir "$HOME_DIR" --batch --pinentry-mode loopback --passphrase '' \
  --quick-gen-key "Quadra Bank <bank@example.com>" ed25519 sign,cert never
BANK=$($GPG --homedir "$HOME_DIR" --list-keys --with-colons bank@example.com | awk -F: '/^fpr/ {print $10; exit}')
$GPG --homedir "$HOME_DIR" --batch --pinentry-mode loopback --passphrase '' \
  --quick-add-key "$BANK" cv25519 encr never
$GPG --homedir "$HOME_DIR" --batch --pinentry-mode loopback --passphrase pgp-pass \
  --armor --export-secret-keys signer@example.com > pgp-signer.sec.asc
$GPG --homedir "$HOME_DIR" --armor --export signer@example.com > pgp-signer.pub.asc
$GPG --homedir "$HOME_DIR" --batch --pinentry-mode loopback --passphrase '' \
  --armor --export-secret-keys bank@example.com > pgp-bank.sec.asc
$GPG --homedir "$HOME_DIR" --armor --export bank@example.com > pgp-bank.pub.asc
rm -rf "$HOME_DIR"
