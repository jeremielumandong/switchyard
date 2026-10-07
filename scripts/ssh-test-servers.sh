#!/usr/bin/env bash
# Start three local OpenSSH servers for the SSH integration tests (Linux, needs root):
# 127.0.0.1:2222 (main) and 2223, 2224 (jump chain). Creates user `swy` (password
# `swypass`) and test keys in $DIR (default /tmp/switchyard-ssh).
#
#   sudo scripts/ssh-test-servers.sh
#   SWITCHYARD_SSH_KEYS=/tmp/switchyard-ssh cargo test -p switchyard-remote --test ssh -- --ignored --test-threads 1
set -euo pipefail
DIR=${DIR:-/tmp/switchyard-ssh}
mkdir -p "$DIR" /run/sshd
id swy >/dev/null 2>&1 || useradd -m -s /bin/bash swy
echo 'swy:swypass' | chpasswd
install -d -m 700 -o swy -g swy /home/swy/.ssh
rm -f "$DIR"/id_* "$DIR"/host_*
for k in ed25519 ecdsa rsa; do ssh-keygen -q -t "$k" -N '' -f "$DIR/id_$k" -C "swy-$k"; done
ssh-keygen -q -t ed25519 -N 'keypass' -f "$DIR/id_ed25519_enc" -C swy-enc
cat "$DIR"/id_*.pub > /home/swy/.ssh/authorized_keys
chown swy:swy /home/swy/.ssh/authorized_keys
chmod 600 /home/swy/.ssh/authorized_keys
chmod 644 "$DIR"/id_* && chmod 600 "$DIR"/id_ed25519 "$DIR"/id_ecdsa "$DIR"/id_rsa "$DIR"/id_ed25519_enc
for p in 2222 2223 2224; do
  ssh-keygen -q -t ed25519 -N '' -f "$DIR/host_$p"
  cat > "$DIR/sshd_$p.conf" <<CONF
Port $p
ListenAddress 127.0.0.1
HostKey $DIR/host_$p
PidFile $DIR/sshd_$p.pid
PasswordAuthentication yes
KbdInteractiveAuthentication yes
PubkeyAuthentication yes
UsePAM yes
AllowTcpForwarding yes
AllowAgentForwarding yes
X11Forwarding yes
X11UseLocalhost yes
PermitRootLogin no
AuthorizedKeysFile .ssh/authorized_keys
Subsystem sftp internal-sftp
LogLevel ERROR
CONF
  [ -f "$DIR/sshd_$p.pid" ] && kill "$(cat "$DIR/sshd_$p.pid")" 2>/dev/null || true
  /usr/sbin/sshd -f "$DIR/sshd_$p.conf" -E "$DIR/sshd_$p.log"
done
# Test keys belong to the user running the tests.
if [ -n "${SUDO_USER:-}" ]; then chown "$SUDO_USER" "$DIR"/id_*; fi
echo "ssh test servers on 127.0.0.1:2222-2224; keys in $DIR"
