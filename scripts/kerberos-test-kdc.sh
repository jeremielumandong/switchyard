#!/usr/bin/env bash
# Throwaway MIT Kerberos realm for the GSSAPI integration test (no system config touched).
#
#   sudo apt-get install -y krb5-kdc krb5-admin-server krb5-user   # Debian/Ubuntu (kadmin.local)
#   eval "$(scripts/kerberos-test-kdc.sh start)"     # prints the env vars to export
#   cargo test -p switchyard-drivers --test gssapi -- --ignored --test-threads 1
#   scripts/kerberos-test-kdc.sh stop
#
# Realm SWITCHYARD.TEST with user swy (password swypass, ticket cached by kinit) and the
# SQL Server service principal MSSQLSvc/db.switchyard.test:1433 in a keytab.
set -euo pipefail

DIR="${KRB5_TEST_DIR:-${TMPDIR:-/tmp}/switchyard-krb5}"
REALM=SWITCHYARD.TEST
PORT="${KRB5_TEST_PORT:-18888}"
SPN="MSSQLSvc/db.switchyard.test:1433"

case "${1:-start}" in
  stop)
    if [[ -f "$DIR/kdc.pid" ]]; then kill "$(cat "$DIR/kdc.pid")" 2>/dev/null || true; fi
    rm -rf "$DIR"
    exit 0 ;;
  start) ;;
  *) echo "usage: $0 [start|stop]" >&2; exit 2 ;;
esac

for tool in krb5kdc kdb5_util kadmin.local kinit; do
  command -v "$tool" >/dev/null || PATH="$PATH:/usr/sbin"
  command -v "$tool" >/dev/null || { echo "$tool not found (install krb5-kdc krb5-admin-server krb5-user)" >&2; exit 1; }
done

if [[ -f "$DIR/kdc.pid" ]]; then kill "$(cat "$DIR/kdc.pid")" 2>/dev/null || true; fi
rm -rf "$DIR" && mkdir -p "$DIR"

cat >"$DIR/krb5.conf" <<EOF
[libdefaults]
  default_realm = $REALM
  dns_lookup_realm = false
  dns_lookup_kdc = false
  rdns = false
[realms]
  $REALM = {
    kdc = 127.0.0.1:$PORT
  }
EOF
cat >"$DIR/kdc.conf" <<EOF
[kdcdefaults]
  kdc_ports = $PORT
  kdc_tcp_ports = $PORT
[realms]
  $REALM = {
    database_name = $DIR/principal
    key_stash_file = $DIR/stash
    acl_file = $DIR/kadm5.acl
  }
[logging]
  kdc = FILE:$DIR/kdc.log
EOF
: >"$DIR/kadm5.acl"

export KRB5_CONFIG="$DIR/krb5.conf" KRB5_KDC_PROFILE="$DIR/kdc.conf"
kdb5_util create -s -r "$REALM" -P "switchyard-master" >&2
kadmin.local -q "addprinc -pw swypass swy" >&2
kadmin.local -q "addprinc -randkey $SPN" >&2
kadmin.local -q "ktadd -k $DIR/service.keytab $SPN" >&2
krb5kdc -P "$DIR/kdc.pid"
export KRB5CCNAME="FILE:$DIR/ccache"
echo swypass | kinit swy >&2

cat <<EOF
export KRB5_CONFIG="$DIR/krb5.conf"
export KRB5CCNAME="FILE:$DIR/ccache"
export KRB5_KTNAME="FILE:$DIR/service.keytab"
export SWITCHYARD_TEST_SPN="$SPN"
EOF
