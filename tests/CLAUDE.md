# e2e tests (`tests/`): notes for Claude Code

Loaded when Claude works on files under `tests/`. Repo-wide rules are in the root `CLAUDE.md`.

- Needs `livekit-server` on PATH or `LIVEKIT_BIN`; Chromium via
  `npx playwright install chromium` or `CHROME_PATH`.
- `E2E_CLI=<path to streamroom-live-cli>` plus `E2E_CLI_ARGS="--height 720 --no-audio"` adds the
  step where the native engine streams into a room (on Linux it needs an X display).
- Clear `HTTPS_PROXY`/`HTTP_PROXY` for local runs behind a proxy.
- Test traps already solved: a closed `<dialog>` never counts as visible (wait on `open`);
  the external-protocol prompt wedges a tab (do that step in its own tab); Chrome needs
  `--use-fake-ui-for-media-stream` to offer host ICE candidates.
