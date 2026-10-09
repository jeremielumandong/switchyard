#!/bin/bash
# Test-only TLS for the compose `mssql` service (run by `mssql-tls`): a throwaway CA and a
# certificate for localhost signed by it, written once to tls/ next to this script
# (git-ignored). Tests trust tls/ca.pem, so certificate verification stays on.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p tls
cd tls
if [ -s ca.pem ] && [ -s server.crt ] && [ -s server.key ] \
  && openssl x509 -checkend 86400 -noout -in server.crt >/dev/null; then
  exit 0
fi
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=Switchyard test CA" \
  -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost,DNS:mssql,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > ext.cnf
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 \
  -extfile ext.cnf -out server.crt 2>/dev/null
# SQL Server runs as uid 10001.
chmod 644 ca.pem server.crt server.key
