# Golden RTSP transcripts

`gst/` holds transcripts of the **GStreamer backend** (neolink 0.6.3-rc.3, default
features) serving the owner's Argus PT Ultra, recorded 2026-10-11 with
`cargo xtask golden record` and normalised with `cargo xtask golden normalize`. The client
is the pinned ffmpeg n8.1.3 (`.github/workflows/test.yml`), `-t 4 -f null`.

They contain no media: interleaved data appears only as one line per channel and payload
type. Credentials are the test users `alice`/`bob` and are normalised away.

| Files | What |
|---|---|
| `open_<alias>_tcp` | All 19 mount aliases, no users configured, TCP interleaved |
| `open_main_tcp_repeat` | A second recording of `/argus/main`; its structural diff against `open_main_tcp` is empty (Gate 0) |
| `open_{main,sub}_udp` | ffmpeg's default transport (UDP) |
| `open_unknown_path` | `/nope/main` → 404 |
| `auth_*` | `[[users]]` alice and bob, `permitted_users = ["alice"]`: none / wrong password / unknown user → 401; bob → 404; alice → 200 |

Known GStreamer-backend behaviour captured here, which the native backend does **not**
copy:

- `open_Main_tcp`: PLAY returned **503** when the client connected right after the
  previous client's TEARDOWN (a media reuse race in the GStreamer factory). The next 17
  opens succeeded.
- `open_extern*`: DESCRIBE returns **400** for a stream the camera doesn't have (the mount
  exists, media creation fails). The native backend returns 404 (SPEC R8).
- Audio is L16/16000/2 (decoded and resampled); the native backend sends AAC (SPEC §5).

The script that recorded them is `docs/native-rtsp/golden-session.sh`.
