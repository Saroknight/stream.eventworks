---
name: deploy
description: Deploy or update the Streamroom site on AWS Lightsail from CloudShell; server file paths and logs for debugging the live box.
---

# Deploy the site

From AWS CloudShell (re-running updates in place and keeps people and settings):
```
rm -rf stream.eventworks && git clone -q --depth 1 https://github.com/Saroknight/stream.eventworks \
  && DOMAIN=stream.eventworks.click bash stream.eventworks/deploy/aws/lightsail.sh
```
Add `REGION`/`NAME` if they were set originally. `RELAY_443=on` turns on the parked relay.
The script creates or reuses the instance + static IP, opens ports 80, 443, 7881/tcp,
7882/udp (and 3478/udp), sets Route 53 DNS if the zone is there, then runs
`deploy/linux/setup.sh <domain> <public ip>` on the box.

On the server: code `/opt/streamroom`, settings `/etc/streamroom.env`, data
`/var/lib/streamroom/state.json`, LiveKit `/etc/livekit/livekit.yaml`; logs
`journalctl -u streamroom -u livekit -u caddy -f`.
