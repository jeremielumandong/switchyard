#!/usr/bin/env bash
# Start moto (an AWS emulator: S3, Secrets Manager, SSM Parameter Store) on 127.0.0.1:5000
# for the cloud integration tests, from a virtualenv in $DIR (default /tmp/switchyard-moto).
#
#   scripts/moto-server.sh
#   cargo test -p switchyard-cloud --test services -- --ignored --test-threads 1
set -euo pipefail
DIR=${DIR:-/tmp/switchyard-moto}
PORT=${PORT:-5000}
if [ ! -x "$DIR/bin/moto_server" ]; then
  python3 -m venv "$DIR"
  "$DIR/bin/pip" install --quiet 'moto[server]==5.1.14'
fi
nohup "$DIR/bin/moto_server" -H 127.0.0.1 -p "$PORT" >"$DIR/moto.log" 2>&1 &
for _ in $(seq 1 60); do
  if curl -fs "http://127.0.0.1:$PORT/moto-api/" >/dev/null; then
    echo "moto listening on 127.0.0.1:$PORT"
    exit 0
  fi
  sleep 1
done
echo "moto did not start; see $DIR/moto.log" >&2
exit 1
