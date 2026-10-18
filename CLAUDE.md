# Just Video: development notes

A Rust VR video player for the Steam Frame (aarch64 SteamOS headset) that plays
from SMB shares and the headset's own storage: OpenXR + Vulkan rendering, FFmpeg
decoding (V4L2 hardware decoder `iris` on the headset), a built-in SMB client in
`third_party/`. `docs/` holds the architecture and measurement notes; `todo.md`
records each finished item with what was done and why.

## Build and test

- PC checks: `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`.
  Needs FFmpeg dev headers (`.cargo/config.toml` points `PKG_CONFIG_PATH` at
  `.local-deps/sysroot`). `readahead::tests::pipelining_hides_latency` is
  timing-flaky under load: rerun it alone before suspecting a real failure.
- UI tests and `ui-preview` need a system font (Noto Sans or DejaVu Sans) and
  Noto Sans CJK (static or variable): on Debian/Ubuntu
  `sudo apt install fonts-noto-core fonts-noto-cjk`.
- `cargo run -- ui-preview <dir>` renders every browser screen to PNGs; look at
  them after changing the UI. There is no OpenXR runtime on the PC, so the
  frame loop itself only runs on the headset.
- Headset build: `bash scripts/build-frame-media.sh` once (FFmpeg + dav1d, with
  the patches in `third_party/ffmpeg-patches/`), then `bash scripts/build-frame.sh`
  (cargo-zigbuild, aarch64, static FFmpeg, fat LTO).
- After changing a patch, rebuild the media libs; the FFmpeg source is
  patched only when first unpacked.

## Deploy and SSH

- `bash scripts/install-frame.sh` copies the binary, launcher and Steam art to
  `~/Applications/JustVideo` on the headset. `FRAME_RESTART_STEAM=1` restarts
  Steam once (needed to set the VR-app flag and icon). It keeps no old
  binary: to roll back, rebuild an older commit.
- `bash scripts/frame-ssh.sh '<command>'` runs a command on the headset as
  `steamos` with the dev key (no sudo). The address defaults to `frame.local`;
  set `FRAME_HOST=<ip>` for the direct link if that doesn't resolve.
- The app logs to `~/.local/state/just-video/log.txt` (previous run in
  `log.previous.txt`; it contains binary bytes, so `grep -a`). The launcher's
  `~/Applications/JustVideo/just-video.log` only catches what is printed
  before that file is opened.
- Don't leave extra binaries (`.prev`, `.test`) on the headset.

## Testing on the headset

- Steam suspends the headset after an hour without input, even on the charger.
  `bash scripts/frame-keep-awake.sh [HOURS]` holds a logind inhibitor (`stop`,
  `status` also work). Caveat: Steam then stays in its sleep power state until
  someone uses the headset. VR apps don't draw, and Steam's web helper keeps a
  second decoder session open (see below), so 8K can't use the hardware
  decoder. 6K and 1080p tests over SSH are unaffected.
- Mute before any playback test:
  `bash scripts/frame-ssh.sh 'wpctl set-mute @DEFAULT_AUDIO_SINK@ 1'`.
- Nobody wears the headset during SSH tests, and SteamVR then doesn't ask for
  frames. `JUST_VIDEO_RENDER_UNSEEN=1` makes the frame loop draw anyway
  (real swapchains, upload, shader, `xrEndFrame`). Launch through Steam to get the
  app's SteamVR settings. While playing, the app logs `Timing: frames ...`
  every 5 s with where the frame time went.
- Reboot without sudo:
  `systemd-run --user --quiet --collect --unit=just-video-reboot systemctl reboot --no-ask-password`
  (run it through `frame-ssh.sh`). Re-run keep-awake after the headset is back.
- Diagnostics without XR: `just-video play-probe <smb-url>` plays a stretch and
  seeks, logging the frame cadence and the time to the first picture after
  each seek; `just-video read-bench <smb-url>` measures raw SMB throughput
  (`--connections 2` compares the two-connection setting); `library-stress`
  exercises the browser's workers.

## Hardware decoder (iris) session load

The `iris` driver adds up the load of every open V4L2 decoder session when one
starts streaming, and counts each as the starting session (its macroblocks per
frame and frame rate, at least 30 fps). Limits: 278528 macroblocks per frame
and 7833600 macroblocks per second. Steam's own web helper is a session too.
So with one other session a new 6K/8K decoder passes, but a flushed one that
played 60 fps content is refused (`qcom-iris: current session not
supported(-12)` in the kernel log); with two other sessions 8K never opens. A
refusal alone isn't a player bug. The decoder is 8-bit only: 10-bit input
crashes its firmware, so 10-bit video decodes on the CPU.

## Benchmarking

- Wi-Fi noise swamps totals. Run alternating pairs (old build, new build,
  old, new, ...) and compare medians, not single runs or means.
- Split timings into phases (network wait, decode, copy, render) before
  deciding what to optimize.
- Check the first run after a reboot or a Steam restart separately: the
  decoder state differs (Steam's helper session).
- Speed of opening, seeking and playback comes first. Anything that reads
  ahead or in the background must give way to the block the player needs now:
  a self-filling read-ahead once saturated the link for minutes after every
  open and was rolled back.

## Writing

- No em-dashes anywhere: code, comments, docs, commit messages. Use commas,
  colons or full stops.
- Comments and commit messages are short, plain sentences that say why.
- Never put private file or folder names, server names, user names, Wi-Fi names,
  passwords or local paths into code, tests, docs or commit messages. Use
  generic names (`a_180_sbs.mp4`, `smb://alice@192.168.1.10`). Grep the staged
  diff before committing.

## Changelog

- Every commit that changes behaviour adds one very brief bullet to
  `CHANGELOG.md`. Sections hold only `## <version> (<date>)` and bullets.
- Bullets go in today's section. The first change on a new day opens a new
  section: bump the minor version in `Cargo.toml` (patch for fix-only days).
