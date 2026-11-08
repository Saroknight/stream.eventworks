# Backlog

Not started unless the owner picks them:
- Security pass items (from the earlier review): pin GitHub Actions to commit SHAs and sign
  updates; shorter LiveKit token TTL than 8h; remove the legacy `/token` and `#k=` app links;
  CSP and HSTS headers on the site; a CSP for the app window; decide on invite-link lifetime and
  reuse; scheduled Lightsail snapshots for backups; on Linux, store the app key encrypted
  (Secret Service/libsecret); today `account.dat` is a plain file with 0600 permissions.
- Windows per-app sound (`activate_process_loopback` in `wincap/src/audio.rs`): on the 5 s
  timeout the function returns while the activation may still be pending, freeing the stack
  `params` its PROPVARIANT blob points at (use-after-free if Windows reads it later). Fix: heap
  allocate `params`/`PROPVARIANT` owned by the `Done` completion handler (Windows holds it until
  `ActivateCompleted`), narrow the whole-function `unsafe` block to the two calls with `SAFETY`
  comments, move the condvar wait into a safe helper (removes the last two `unwrap()`s that fail
  Windows-target clippy). Own branch; needs testing on Windows.
- Port 443 relay: parked; revisit only if someone's network actually blocks the video ports.
- Linux: confirm 0.4.7+ streams cleanly at the set fps on the tester's GNOME machine, including
  per-app sound. No Flatpak/other distros planned; Steam Deck and OBS integration explicitly out
  of scope.
- `appinfo.js` changes and the Linux download command need a Lightsail redeploy to go live.

