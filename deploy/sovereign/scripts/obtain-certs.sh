#!/usr/bin/env bash
# Obtain a Let's Encrypt certificate for the nDB edge and stage it where
# ndb-edge expects it (/etc/ndb/{fullchain,privkey}.pem).
#
# Usage:  sudo ./obtain-certs.sh ndb.example.com you@example.com
#
# Prereqs: a DNS A record for the domain pointing at this box, and port 80
# reachable from the internet for the standalone challenge (open it briefly).
set -euo pipefail

DOMAIN="${1:?usage: obtain-certs.sh <domain> <email>}"
EMAIL="${2:?usage: obtain-certs.sh <domain> <email>}"

if ! command -v certbot >/dev/null 2>&1; then
  echo "installing certbot..."
  apt-get update -y && apt-get install -y certbot
fi

echo "obtaining cert for ${DOMAIN} (standalone, needs :80 free + open)..."
certbot certonly --standalone --non-interactive --agree-tos \
  -m "${EMAIL}" -d "${DOMAIN}"

LIVE="/etc/letsencrypt/live/${DOMAIN}"
install -d -m 0755 /etc/ndb
# ndb-edge reads these two paths (deref the symlinks so the service user can read them)
cp -L "${LIVE}/fullchain.pem" /etc/ndb/fullchain.pem
cp -L "${LIVE}/privkey.pem"   /etc/ndb/privkey.pem
chown ndb:ndb /etc/ndb/fullchain.pem /etc/ndb/privkey.pem
chmod 0640 /etc/ndb/fullchain.pem /etc/ndb/privkey.pem

# Renewal hook: re-copy + restart the edge after certbot renews (runs via the
# certbot systemd timer). Pingora reads cert files at startup, so it must restart.
HOOK=/etc/letsencrypt/renewal-hooks/deploy/ndb-edge.sh
install -d -m 0755 "$(dirname "$HOOK")"
cat > "$HOOK" <<HOOK_EOF
#!/usr/bin/env bash
set -e
cp -L "${LIVE}/fullchain.pem" /etc/ndb/fullchain.pem
cp -L "${LIVE}/privkey.pem"   /etc/ndb/privkey.pem
chown ndb:ndb /etc/ndb/fullchain.pem /etc/ndb/privkey.pem
chmod 0640 /etc/ndb/fullchain.pem /etc/ndb/privkey.pem
systemctl restart ndb-edge
HOOK_EOF
chmod 0755 "$HOOK"

echo "done. certs at /etc/ndb/, renewal hook installed at ${HOOK}"
