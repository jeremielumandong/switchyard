#!/usr/bin/env bash
# TLS certificate for the FTP test servers (docker/compose.yml: ftp, ftp-implicit), signed
# by a throwaway CA so the tests run with certificate verification on.
#
#   scripts/ftp-test-certs.sh
#   docker compose -f docker/compose.yml up -d ftp ftp-implicit ftp-plain
#   SWITCHYARD_FTP_CA=docker/ftp/ca.pem \
#     cargo test -p switchyard-remote --test ftp -- --ignored --test-threads 1
set -euo pipefail
DIR=${DIR:-$(cd "$(dirname "$0")/../docker/ftp" && pwd)}
cd "$DIR"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=Switchyard FTP test CA" \
  -keyout "$TMP/ca.key" -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout key.pem -out "$TMP/server.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' > "$TMP/ext.cnf"
openssl x509 -req -in "$TMP/server.csr" -CA ca.pem -CAkey "$TMP/ca.key" -CAcreateserial \
  -CAserial "$TMP/ca.srl" -days 30 -extfile "$TMP/ext.cnf" -out cert.pem 2>/dev/null
# vsftpd runs as root in the container; the key only needs to be readable there.
chmod 644 cert.pem ca.pem
chmod 600 key.pem
echo "FTP test certificate in $DIR (CA: $DIR/ca.pem)"
