# Scheduled maintenance templates

These are starting points for running `opencode-dbtool cleanup` on a
schedule. Adjust the binary path and retention policy to taste.

## systemd (Linux)

```sh
mkdir -p ~/.config/systemd/user
cp contrib/systemd/opencode-dbtool-cleanup.* ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now opencode-dbtool-cleanup.timer
systemctl --user list-timers opencode-dbtool-cleanup.timer
```

## launchd (macOS)

```sh
cp contrib/launchd/com.orrisroot.opencode-dbtool-cleanup.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.orrisroot.opencode-dbtool-cleanup.plist
```

## Notes

- `--restart-service` stops and restarts the registered opencode service
  around the run; without it the service keeps running and the final
  VACUUM is skipped (see the README's "Online maintenance" section).
- `--yes` is required for non-interactive runs.
- Point `OPENCODE_DB` / `OPENCODE_DATA_DIR` at the right installation if
  you use non-default paths (environment variables must be set for the
  timer/agent too).
