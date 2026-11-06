#!/usr/bin/env bash
# Streamroom installer / updater for Arch Linux and Ubuntu/Debian.
#
#   sudo bash deploy/linux/setup.sh stream.example.com [public-ipv4]
#
# public-ipv4: the server's fixed public IPv4 address, which browsers send video to. Give it
# whenever you know it (the Lightsail script does). Without it, LiveKit looks it up via STUN at
# start, which can pick up a stale or wrong address.
#
# Run it from the project folder. Safe to re-run: it updates the app code and restarts
# services, but keeps your existing keys, admin password and rooms.
set -euo pipefail

DOMAIN="${1:-}"
PUBLIC_IP="${2:-${PUBLIC_IP:-}}"
if [[ -z "$DOMAIN" ]]; then
  echo "Usage: sudo $0 your.domain.com" >&2
  exit 1
fi
if [[ $EUID -ne 0 ]]; then
  echo "Please run with sudo." >&2
  exit 1
fi

SRC="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$SRC/deploy/linux"
APP=/opt/streamroom
ENVFILE=/etc/streamroom.env

say() { printf '\n\033[1;31m●\033[0m %s\n' "$*"; }

say "Installing packages (Node.js, Caddy, openssl, rsync)"
if command -v pacman >/dev/null 2>&1; then
  pacman -S --needed --noconfirm nodejs npm caddy openssl rsync curl
elif command -v apt-get >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -q
  apt-get install -y -q ca-certificates curl gnupg openssl rsync caddy
  # Distro Node.js is often too old; use the NodeSource build of Node 22.
  if ! node -e 'process.exit(+process.versions.node.split(".")[0] < 20)' 2>/dev/null; then
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
    apt-get install -y -q nodejs
  fi
else
  echo "Unsupported system: needs pacman (Arch) or apt (Ubuntu/Debian)." >&2
  exit 1
fi
node --version

if ! command -v livekit-server >/dev/null 2>&1; then
  say "Installing LiveKit server"
  curl -sSL https://get.livekit.io | bash
fi
livekit-server --version

say "Creating service users"
NOLOGIN="$(command -v nologin || echo /usr/sbin/nologin)"
id livekit    >/dev/null 2>&1 || useradd --system --no-create-home --user-group --shell "$NOLOGIN" livekit
id streamroom >/dev/null 2>&1 || useradd --system --no-create-home --user-group --shell "$NOLOGIN" streamroom

say "Copying app to $APP"
mkdir -p "$APP"
rsync -a --delete --exclude node_modules --exclude data --exclude .env --exclude .git --exclude app "$SRC/" "$APP/"
(cd "$APP" && npm ci --omit=dev --no-audit --no-fund)

# ---- secrets: generate once, keep on re-runs ----
if [[ -f "$ENVFILE" ]]; then
  say "Keeping existing secrets in $ENVFILE"
  # shellcheck disable=SC1090
  source "$ENVFILE"
else
  say "Generating LiveKit keys"
  LIVEKIT_API_KEY="API$(openssl rand -hex 6)"
  LIVEKIT_API_SECRET="$(openssl rand -base64 48 | tr -dc 'A-Za-z0-9' | head -c 48)"
fi
# Signs invite, device and admin links. Added on first run (and on upgrades from older versions).
AUTH_SECRET="${AUTH_SECRET:-$(openssl rand -base64 48 | tr -dc 'A-Za-z0-9' | head -c 48)}"

cat > "$ENVFILE" <<EOF
PORT=3000
PUBLIC_URL=https://$DOMAIN
LIVEKIT_API_KEY=$LIVEKIT_API_KEY
LIVEKIT_API_SECRET=$LIVEKIT_API_SECRET
LIVEKIT_PUBLIC_URL=wss://$DOMAIN
AUTH_SECRET=$AUTH_SECRET
LIVEKIT_NODE_IP=$PUBLIC_IP
EOF
chown root:streamroom "$ENVFILE"
chmod 640 "$ENVFILE"

# One command to print a fresh admin sign-in link (first login, or if you're ever locked out).
cat > /usr/local/bin/streamroom-admin-link <<'EOF'
#!/usr/bin/env bash
set -a; source /etc/streamroom.env; set +a
export DATA_DIR=/var/lib/streamroom
exec /usr/bin/node /opt/streamroom/server/cli.js admin-link "$@"
EOF
chmod 750 /usr/local/bin/streamroom-admin-link

say "Writing LiveKit config"
mkdir -p /etc/livekit
if [[ -n "$PUBLIC_IP" ]]; then
  # Advertise exactly this address for media instead of asking a STUN server.
  IPLINE="node_ip: $PUBLIC_IP"
  echo "Media address: $PUBLIC_IP"
else
  IPLINE="use_external_ip: true"
  echo "Media address: looked up via STUN when LiveKit starts"
fi
sed -e "s|__API_KEY__|$LIVEKIT_API_KEY|" -e "s|__API_SECRET__|$LIVEKIT_API_SECRET|" \
  -e "s|^  __PUBLIC_IP_LINE__.*|  $IPLINE|" \
  "$HERE/livekit.yaml" > /etc/livekit/livekit.yaml
chown root:livekit /etc/livekit/livekit.yaml
chmod 640 /etc/livekit/livekit.yaml

# ---- port 443 fallback (video relay) ----
# For viewers whose network blocks the normal video ports: LiveKit's built-in TURN relay,
# reached on port 443 as turn.<domain>. Caddy shares 443 between the site and the relay, which
# needs the Caddy build with the layer4 module. Any problem here: skip it and keep the
# normal setup working.
TURN_DOMAIN="turn.$DOMAIN"
RELAY=off
CADDY_L4=/usr/local/bin/caddy-streamroom
resolve4() { getent ahostsv4 "$1" 2>/dev/null | awk 'NR==1{print $1}'; }
install_caddy_l4() {
  if [[ -x "$CADDY_L4" ]] && "$CADDY_L4" list-modules 2>/dev/null | grep -q '^layer4$'; then return 0; fi
  local arch; case "$(uname -m)" in x86_64) arch=amd64 ;; aarch64|arm64) arch=arm64 ;; *) return 1 ;; esac
  say "Downloading Caddy with the layer4 module (for the port 443 video relay)"
  local tmp; tmp="$(mktemp)"
  if curl -fsSL --retry 3 -o "$tmp" "https://caddyserver.com/api/download?os=linux&arch=$arch&p=github.com%2Fmholt%2Fcaddy-l4" \
     && chmod 755 "$tmp" && "$tmp" list-modules 2>/dev/null | grep -q '^layer4$'; then
    install -m 755 "$tmp" "$CADDY_L4"; rm -f "$tmp"; return 0
  fi
  rm -f "$tmp"; return 1
}
caddy_is_ours() {
  # No config yet, ours, the package's stock welcome page, or just our import line.
  [[ ! -f /etc/caddy/Caddyfile ]] || grep -q 'Managed by Streamroom\|/usr/share/caddy' /etc/caddy/Caddyfile \
    || [[ "$(grep -v '^[[:space:]]*\(#\|$\)' /etc/caddy/Caddyfile)" == 'import /etc/caddy/conf.d/*' ]]
}
if [[ "${RELAY_443:-on}" == off ]]; then
  echo "Port 443 relay: turned off (RELAY_443=off)"
elif [[ -z "$(resolve4 "$TURN_DOMAIN")" || "$(resolve4 "$TURN_DOMAIN")" != "${PUBLIC_IP:-$(resolve4 "$DOMAIN")}" ]]; then
  echo "Port 443 relay: skipped. To enable it, point $TURN_DOMAIN at this server (same address as $DOMAIN) and re-run."
elif ! caddy_is_ours; then
  echo "Port 443 relay: skipped, because /etc/caddy/Caddyfile has your own settings."
elif ! install_caddy_l4; then
  echo "Port 443 relay: skipped, couldn't get the Caddy layer4 build. The site works as before."
else
  RELAY=on
fi
if [[ $RELAY == on ]]; then
  cat >> /etc/livekit/livekit.yaml <<EOF
turn:
  enabled: true
  domain: $TURN_DOMAIN
  tls_port: 5349            # plain TURN; Caddy does the TLS on 443 and forwards here
  external_tls: true
  udp_port: 3478            # TURN over UDP too  -> open 3478/udp in the firewall
EOF
  echo "TURN_DOMAIN=$TURN_DOMAIN" >> "$ENVFILE"
fi

say "Writing Caddy site for $DOMAIN"
mkdir -p /etc/caddy/conf.d
CADDY_DROPIN=/etc/systemd/system/caddy.service.d/streamroom.conf
CADDY_BIN_BEFORE="$(grep -s -o 'caddy-streamroom' "$CADDY_DROPIN" || true)"
use_plain_caddy() {
  sed "s|__DOMAIN__|$DOMAIN|" "$HERE/Caddyfile" > /etc/caddy/conf.d/streamroom.caddy
  if [[ ! -f /etc/caddy/Caddyfile ]] || grep -q 'Managed by Streamroom' /etc/caddy/Caddyfile; then
    echo 'import /etc/caddy/conf.d/*' > /etc/caddy/Caddyfile
  elif ! grep -q 'conf.d' /etc/caddy/Caddyfile; then
    cp /etc/caddy/Caddyfile "/etc/caddy/Caddyfile.bak.$(date +%s)"
    if grep -q '/usr/share/caddy' /etc/caddy/Caddyfile; then
      # Stock "welcome page" config from the package: replace it, it would grab port 80.
      echo 'import /etc/caddy/conf.d/*' > /etc/caddy/Caddyfile
    else
      # Your own Caddy config: keep it and add ours alongside.
      printf '\nimport /etc/caddy/conf.d/*\n' >> /etc/caddy/Caddyfile
    fi
  fi
  rm -f "$CADDY_DROPIN"
  caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
}
if [[ $RELAY == on ]]; then
  cp -f /etc/caddy/Caddyfile "/etc/caddy/Caddyfile.prev" 2>/dev/null || true
  sed -e "s|__DOMAIN__|$DOMAIN|g" -e "s|__TURN_DOMAIN__|$TURN_DOMAIN|g" -e "s|__GLOBAL_EXTRA__|${CADDY_GLOBAL_EXTRA:-}|" \
    "$HERE/Caddyfile.relay" > /etc/caddy/Caddyfile.new
  if "$CADDY_L4" validate --config /etc/caddy/Caddyfile.new --adapter caddyfile; then
    mv /etc/caddy/Caddyfile.new /etc/caddy/Caddyfile
    rm -f /etc/caddy/conf.d/streamroom.caddy
    mkdir -p "$(dirname "$CADDY_DROPIN")"
    cat > "$CADDY_DROPIN" <<EOF
# Streamroom: run the Caddy build with the layer4 module (port 443 shared with the video relay).
[Service]
ExecStart=
ExecStart=$CADDY_L4 run --environ --config /etc/caddy/Caddyfile
ExecReload=
ExecReload=$CADDY_L4 reload --config /etc/caddy/Caddyfile --force
EOF
    echo "Port 443 relay: on ($TURN_DOMAIN)"
  else
    rm -f /etc/caddy/Caddyfile.new
    echo "Port 443 relay: the Caddy config didn't validate, so it's off. The site works as before."
    RELAY=off
    sed -i '/^turn:/,$d' /etc/livekit/livekit.yaml
    sed -i '/^TURN_DOMAIN=/d' "$ENVFILE"
    use_plain_caddy
  fi
else
  use_plain_caddy
fi
CADDY_BIN_AFTER="$(grep -s -o 'caddy-streamroom' "$CADDY_DROPIN" || true)"

say "Installing systemd services"
install -m 644 "$HERE/livekit.service"    /etc/systemd/system/livekit.service
install -m 644 "$HERE/streamroom.service" /etc/systemd/system/streamroom.service
systemctl daemon-reload
systemctl enable --now livekit streamroom caddy
systemctl restart livekit streamroom
if [[ "$CADDY_BIN_BEFORE" != "$CADDY_BIN_AFTER" ]]; then
  systemctl restart caddy # switching Caddy builds needs a restart (a second or two offline)
else
  systemctl reload caddy || systemctl restart caddy
fi

say "Firewall"
PORTS_MSG="Open these ports: 80/tcp, 443/tcp, 7881/tcp, 7882/udp, 3478/udp"
if command -v ufw >/dev/null 2>&1 && ufw status | grep -q active; then
  ufw allow 80/tcp; ufw allow 443/tcp; ufw allow 7881/tcp; ufw allow 7882/udp; ufw allow 3478/udp
  echo "ufw rules added."
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port=80/tcp --add-port=443/tcp --add-port=7881/tcp --add-port=7882/udp --add-port=3478/udp
  firewall-cmd --reload
  echo "firewalld rules added."
else
  echo "No active ufw/firewalld found. $PORTS_MSG (also on your cloud provider's firewall, if any)."
fi

echo
echo "=============================================================="
echo " Streamroom is running:  https://$DOMAIN"
if /usr/local/bin/streamroom-admin-link --if-no-admin >/tmp/streamroom-admin-link.txt 2>/dev/null; then
  echo " First time? Sign in as admin with this one-time link (valid 24 hours):"
  grep -o 'https://[^ ]*' /tmp/streamroom-admin-link.txt | sed 's/^/   /'
else
  echo " Admin account already set up. Need a new admin sign-in link? Run: sudo streamroom-admin-link"
fi
rm -f /tmp/streamroom-admin-link.txt
echo " Make sure $DOMAIN points (DNS A record) at this server."
echo " $PORTS_MSG"
echo " Logs:  journalctl -u streamroom -u livekit -f"
echo "=============================================================="
