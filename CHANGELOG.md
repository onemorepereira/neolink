# Changelog

Notable changes to this fork of neolink. The baseline is upstream commit `6e05e78`.

## Unreleased

### Docker

- **neolink is the image's entrypoint and PID 1.** `docker stop` now logs the cameras out
  and exits 0 within a second; before, the shell entrypoint swallowed SIGTERM and the
  container was killed after 10 s. The command may be neolink's arguments (new:
  `--config ...`, `rtsp --config ...`) or, as before, `neolink ...` or any other program
  (`sh -c "..."`, a script), which neolink runs in its place. A neolink subcommand wins
  over a program of the same name (`users`). `/entrypoint.sh` remains for setups that
  name it. In a `sh -c` command, end with `exec neolink ...` so that the stop reaches
  neolink.
- **`NEO_LINK_MODE` applies whenever the command has no subcommand**, including
  `neolink --config ...`; before, that form always ran `rtsp`. An option value that
  happens to be a subcommand name (`--config rtsp`) is no longer taken for one.
- **State directory.** `/var/lib/neolink` (`NEOLINK_STATE_DIR`) is a volume. With
  `PUID`/`PGID`, neolink hands the state directory and the root-owned files it wrote
  there as root (`camera/`, `cache/`, `learn-*`) to that user, never following symlinks
  and never the filesystem root. At startup it checks that the directory and `camera/`
  are writable and logs an error otherwise (also with `--user`).
- neolink stops at startup if it cannot install its SIGTERM/SIGINT handlers, instead of
  running deaf to `docker stop`.
- The kubernetes example ends its `sh -c` with `exec`.
- `tests/e2e/container-entry.sh` runs these cases against a built image.
