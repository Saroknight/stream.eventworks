#!/usr/bin/env bash
# shellcheck disable=SC2016  # backticks in --query strings are JMESPath literals
# Streamroom on Amazon Lightsail, run from AWS CloudShell.
#
# Open CloudShell in the AWS console and run (this pulls the latest code from GitHub):
#
#   rm -rf stream.eventworks && git clone -q --depth 1 https://github.com/Saroknight/stream.eventworks \
#     && DOMAIN=stream.example.com bash stream.eventworks/deploy/aws/lightsail.sh
#
# Settings (optional, put them before the command, e.g. REGION=us-west-2 bash ...):
#   REGION   AWS region for the server              (default: CloudShell's region, else us-east-1)
#   DOMAIN   your own domain or subdomain, e.g. stream.example.com (default: free <ip>.sslip.io name).
#            If the domain is in Route 53, the DNS record is created for you. Otherwise the script
#            prints the IP to point it at and waits until DNS is set up.
#   NAME     name for the instance and static IP    (default: streamroom)
#   PLAN_GB  RAM of the Lightsail plan: 1, 2 or 4   (default: 2, the $12/month plan)
#
# Re-running it is safe: it reuses the existing server and just updates the app.
set -euo pipefail

REGION="${REGION:-${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}}"
NAME="${NAME:-streamroom}"
IPNAME="${NAME}-ip"
PLAN_GB="${PLAN_GB:-2}"
# The code to deploy: the checkout this script lives in.
SRC="$(cd "$(dirname "$0")/../.." && pwd)"
KEY="$HOME/.ssh/lightsail-${REGION}.pem"
export AWS_DEFAULT_REGION="$REGION" AWS_PAGER=""

say() { printf '\n\033[1;31m●\033[0m %s\n' "$*"; }
ls_() { aws lightsail "$@"; }

[[ -f "$SRC/server/index.js" && -f "$SRC/deploy/linux/setup.sh" ]] || { echo "Run this from a checkout of the Streamroom repository." >&2; exit 1; }
command -v jq >/dev/null || sudo yum install -y -q jq

say "Using region $REGION"
aws sts get-caller-identity --query Account --output text >/dev/null

# ---- 1. instance ----
if ls_ get-instance --instance-name "$NAME" >/dev/null 2>&1; then
  say "Instance '$NAME' already exists, reusing it"
else

  BLUEPRINT=$(ls_ get-blueprints --query 'blueprints[?isActive && platform==`LINUX_UNIX`].blueprintId' --output json \
    | jq -r '[.[] | select(test("^ubuntu_2[0-9]_04$"))] | sort | last')
  BUNDLE=$(ls_ get-bundles --query 'bundles[?isActive]' --output json \
    | jq -r --argjson gb "$PLAN_GB" '[.[] | select(.ramSizeInGb == $gb and (.supportedPlatforms | index("LINUX_UNIX"))
        and (.bundleId | test("ipv6") | not) and (.instanceType != null))] | sort_by(.price) | .[0].bundleId')
  [[ "$BLUEPRINT" != null && "$BUNDLE" != null ]] || { echo "Couldn't find an Ubuntu image or a ${PLAN_GB} GB plan in $REGION." >&2; exit 1; }
  ZONE=$(aws ec2 describe-availability-zones --region "$REGION" --query 'AvailabilityZones[0].ZoneName' --output text 2>/dev/null || echo "${REGION}a")
  PRICE=$(ls_ get-bundles --query "bundles[?bundleId=='$BUNDLE'].price | [0]" --output text)
  say "Creating instance '$NAME' ($BLUEPRINT, plan $BUNDLE, \$$PRICE/month) in $ZONE"
  ls_ create-instances --instance-names "$NAME" --availability-zone "$ZONE" \
    --blueprint-id "$BLUEPRINT" --bundle-id "$BUNDLE" --tags key=app,value=streamroom >/dev/null
fi

printf 'Waiting for the instance to start'
until [[ "$(ls_ get-instance-state --instance-name "$NAME" --query 'state.name' --output text)" == running ]]; do
  printf '.'; sleep 5
done
echo " running"

# ---- 2. static IP (free while attached) ----
if ! ls_ get-static-ip --static-ip-name "$IPNAME" >/dev/null 2>&1; then
  say "Allocating static IP '$IPNAME'"
  ls_ allocate-static-ip --static-ip-name "$IPNAME" >/dev/null
fi
if [[ "$(ls_ get-static-ip --static-ip-name "$IPNAME" --query 'staticIp.attachedTo' --output text)" != "$NAME" ]]; then
  ls_ attach-static-ip --static-ip-name "$IPNAME" --instance-name "$NAME" >/dev/null
  sleep 5
fi
IP=$(ls_ get-static-ip --static-ip-name "$IPNAME" --query 'staticIp.ipAddress' --output text)
say "Static IP: $IP"

# ---- 3. firewall ----
say "Opening ports 22, 80, 443, 7881 (TCP) and 7882 (UDP)"
ls_ put-instance-public-ports --instance-name "$NAME" --port-infos \
  fromPort=22,toPort=22,protocol=tcp \
  fromPort=80,toPort=80,protocol=tcp \
  fromPort=443,toPort=443,protocol=tcp \
  fromPort=7881,toPort=7881,protocol=tcp \
  fromPort=7882,toPort=7882,protocol=udp >/dev/null

# ---- 4. SSH key (Lightsail's default key for this region) ----
if [[ ! -f "$KEY" ]]; then
  mkdir -p "$HOME/.ssh"
  ls_ download-default-key-pair --query privateKeyBase64 --output text > "$KEY"
  chmod 600 "$KEY"
fi
SSH=(ssh -i "$KEY" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -o ServerAliveInterval=30 "ubuntu@$IP")

printf 'Waiting for SSH'
until "${SSH[@]}" true 2>/dev/null; do printf '.'; sleep 5; done
echo " ready"

# ---- 5. domain ----
resolves_to_ip() { [[ "$(getent ahostsv4 "$1" 2>/dev/null | awk 'NR==1{print $1}')" == "$IP" ]]; }
if [[ -z "${DOMAIN:-}" ]]; then
  DOMAIN="${IP//./-}.sslip.io"
  say "No DOMAIN given, using free hostname $DOMAIN"
else
  DOMAIN="${DOMAIN%.}"
  # Route 53: find the most specific hosted zone that contains the domain and upsert an A record.
  ZONE_ID=$(aws route53 list-hosted-zones --query 'HostedZones[?Config.PrivateZone==`false`].[Id,Name]' --output json 2>/dev/null \
    | jq -r --arg d "$DOMAIN." '[.[] | select(.[1] as $z | ($d == $z) or ($d | endswith("." + $z)))] | sort_by(.[1] | length) | last | .[0] // empty' || true)
  if [[ -n "$ZONE_ID" ]]; then
    say "Pointing $DOMAIN at $IP in Route 53"
    aws route53 change-resource-record-sets --hosted-zone-id "$ZONE_ID" --change-batch "{\"Changes\":[{\"Action\":\"UPSERT\",
      \"ResourceRecordSet\":{\"Name\":\"$DOMAIN\",\"Type\":\"A\",\"TTL\":60,\"ResourceRecords\":[{\"Value\":\"$IP\"}]}}]}" >/dev/null
  elif ! resolves_to_ip "$DOMAIN"; then
    say "Action needed: at your domain provider, create this DNS record"
    echo "    Type: A    Name: $DOMAIN    Value: $IP    (TTL: lowest allowed)"
    echo "  The script continues automatically once the record is live."
  fi
  printf 'Waiting for %s to point at %s' "$DOMAIN" "$IP"
  for i in $(seq 1 360); do
    resolves_to_ip "$DOMAIN" && break
    [[ $i -eq 360 ]] && { echo; echo "DNS still not pointing at $IP after 30 minutes. Fix the record and re-run this script." >&2; exit 1; }
    printf '.'; sleep 5
  done
  echo " done"
fi

# ---- 6. copy app and run setup ----
say "Copying Streamroom to the server"
BUNDLE="$(mktemp --suffix=.tgz)"
# Web app and server only: the Windows app source and local data stay behind.
tar -czf "$BUNDLE" -C "$SRC" --exclude=./.git --exclude=./app --exclude=./node_modules --exclude=./data --exclude=./.env .
scp -q -i "$KEY" -o StrictHostKeyChecking=accept-new "$BUNDLE" "ubuntu@$IP:streamroom.tgz"
rm -f "$BUNDLE"

say "Installing on the server (takes a few minutes the first time)"
"${SSH[@]}" "set -e
  cloud-init status --wait >/dev/null 2>&1 || true
  rm -rf ~/streamroom && mkdir ~/streamroom && tar -xzf ~/streamroom.tgz -C ~/streamroom
  sudo bash ~/streamroom/deploy/linux/setup.sh '$DOMAIN'"

# ---- 7. check ----
say "Checking https://$DOMAIN"
for _ in $(seq 1 24); do
  if curl -fsS -m 10 "https://$DOMAIN/api/config" >/dev/null 2>&1; then
    echo "It's up: https://$DOMAIN"
    exit 0
  fi
  sleep 5
done
echo "The server is installed but https://$DOMAIN didn't answer yet. The certificate can take a minute;"
echo "try the address in your browser shortly. Logs: ssh in and run  journalctl -u caddy -u streamroom -f"
