#!/usr/bin/env bash
# Start SQL Server in docker for the SQL Server integration tests, with a TLS certificate
# signed by a throwaway CA (so the tests run with certificate verification on).
#
#   scripts/mssql-test-server.sh
#   SWITCHYARD_MSSQL_CA=/tmp/switchyard-mssql/ca.pem \
#     cargo test -p switchyard-db --test mssql -- --ignored --test-threads 1
set -euo pipefail
DIR=${DIR:-/tmp/switchyard-mssql}
IMAGE=${IMAGE:-mcr.microsoft.com/mssql/server:2022-latest}
PASSWORD='Switchyard!2026'
mkdir -p "$DIR"
cd "$DIR"
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=Switchyard test CA" \
  -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > ext.cnf
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 \
  -extfile ext.cnf -out server.crt 2>/dev/null
cat > mssql.conf <<CONF
[network]
tlscert = /var/opt/mssql/tls/server.crt
tlskey = /var/opt/mssql/tls/server.key
tlsprotocols = 1.2
forceencryption = 0
CONF
# The container runs as uid 10001.
chmod 644 server.crt server.key mssql.conf
docker rm -f switchyard-mssql >/dev/null 2>&1 || true
docker run -d --name switchyard-mssql -p 1433:1433 \
  -e ACCEPT_EULA=Y -e MSSQL_PID=Developer -e "MSSQL_SA_PASSWORD=$PASSWORD" \
  -v "$DIR/server.crt:/var/opt/mssql/tls/server.crt:ro" \
  -v "$DIR/server.key:/var/opt/mssql/tls/server.key:ro" \
  -v "$DIR/mssql.conf:/var/opt/mssql/mssql.conf:ro" \
  "$IMAGE" >/dev/null
for _ in $(seq 1 60); do
  if docker exec switchyard-mssql /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa \
      -P "$PASSWORD" -Q 'SELECT 1' >/dev/null 2>&1; then
    echo "SQL Server is up; CA at $DIR/ca.pem"
    exit 0
  fi
  sleep 2
done
docker logs switchyard-mssql | tail -40
exit 1
