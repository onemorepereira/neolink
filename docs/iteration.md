# Iterating this fork safely

This is a personal fork of [`QuantumEntangledAndy/neolink`](https://github.com/QuantumEntangledAndy/neolink) (itself a fork of [`thirtythreeforty/neolink`](https://github.com/thirtythreeforty/neolink)), baseline upstream commit `6e05e78`. The remote `origin` is your own fork (`github.com/onemorepereira/neolink.git`).

## Branching

- Branch off `master`: `git checkout -b type/short-description` (e.g. `fix/...`, `feat/...`, `docs/...`).
- Use [Conventional Commits](https://www.conventionalcommits.org/) (`type(scope): description`), no AI attribution.
- Never commit or push directly to `master`. Open a PR, let CI run, then merge.
- Keep the `feature/native-rtsp-*` branches on origin as history; don't rebase them.

## Gate before every push / PR

The CI (`test.yml`) runs on every PR and push to `master`. Run the same checks locally first:

```bash
# Default (GStreamer) backend — needs libgstreamer1.0-dev etc. installed
cargo test --locked --workspace

# Native backend (no GStreamer; what the k3s image uses)
cargo test --locked --workspace --features native-rtsp,pushnoti

# Retina client tests (own lockfile)
cargo test --locked --manifest-path crates/rtsp-it/Cargo.toml

# Lint + fmt (style_checks.yml)
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Fixture reproducibility (test.yml generates fixtures and --checks them)
cargo xtask mkfixture --standard --out fixtures/synth --check
```

If you touch the native RTSP server, also run the e2e scripts (`tests/e2e/clients.sh`, `golden-native.sh`, `avsync.sh`, `power.sh`) — they need ffmpeg, mosquitto, and the pinned ffmpeg from `.github/actions/setup-ffmpeg`.

## Releases

| Tag pattern | What CI produces | Publishes |
|-------------|------------------|-----------|
| `v*` (e.g. `v0.6.3`) | Multi-platform GStreamer binaries + a **draft** GitHub release (`build.yml`) | Draft only — review, then publish |
| `v*-native*` (e.g. `v0.6.3-native.1`) | Static musl binaries (x86_64 + aarch64) + `FROM scratch` image tarball as workflow artifacts (`native-release.yml`) | Artifacts only (90-day retention); download and release by hand |

Version comes from `Cargo.toml` (`0.6.3-rc.3` now). `build.rs` embeds the commit into `--version`; without a `v*` tag it prints `<version> (<commit>)`.

MSRV is 1.88.0 (`rust-version = "1.88"`). The `cross` job in `build.yml` uses 1.88.0.

## Native backend (beta) — the k3s-friendly build

The native RTSP backend (`--no-default-features --features native-rtsp`) is a single static binary, no GStreamer/OpenSSL, ~15 MB `FROM scratch` image. It is in beta (`docs/native-rtsp/STATUS.md`): hardware gates 4–6 are pending. The GStreamer backend remains the default and is the proven option.

Build it:
```sh
cargo build --release --locked --no-default-features --features native-rtsp
./target/release/neolink rtsp --config neolink.toml
```

For k3s, build the native image with Podman and push it to your private registry (`registry.home.devoops.co/neolink:<sha>`), then pin that SHA in `cluster/neolink/helmrelease.yml` (see `../k3s/cluster/neolink/`).

## Cameras and power (read before deploying)

- Cameras **sleep by default**: neolink connects only while something uses them. Mark every mains/PoE/NVR camera `battery = false` in `[[cameras]]`; leave battery/solar cameras unset.
- The **state directory** (`/var/lib/neolink` in the image, `NEOLINK_STATE_DIR`) holds the wake ledger, last snapshot, and stream cache. Mount a volume there so it survives restarts — otherwise battery cameras are re-woken to relearn streams after every bounce.
- Config is read at startup from `/etc/neolink.toml` (`--config`). Camera passwords and MQTT credentials must not be committed; in k3s they live in a SOPS-encrypted Secret (`cluster/neolink/secret.yml`).
- `NEO_LINK_MODE` selects the mode (`rtsp` | `mqtt` | `mqtt-rtsp`) when no subcommand is given; the image CMD is `--config /etc/neolink.toml`.
- `PUID`/`PGID` drop to that user after the config is read; neolink hands the state directory to that user first.

## k3s deployment

See `../k3s/cluster/neolink/` — a bespoke Helm chart following the cluster's `doorwatch` pattern, registered in `infrastructure/flux-kustomizations.yml`. Push to the k3s repo's `main`; Flux reconciles.
