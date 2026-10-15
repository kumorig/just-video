Patches applied to FFmpeg by `scripts/build-frame-media.sh` (Steam Frame build only).

- `0001-aarch64-hevc-10bit-pel-pixels-neon.patch`: NEON versions of the 10-bit
  HEVC full-sample MC functions (`pel_pixels`, `pel_bi_pixels`, `pel_uni_w_pixels`),
  which FFmpeg 8.1.3 (and master as of 2026-09) only implements in C at 10-bit.
  Verified bit-exact with `checkasm --test=hevc_pel` (376/376) on the headset.
  Steam Frame: 4K60 HEVC Main10 142 → 178 fps; 8K60 unchanged (memory-latency bound).
  Upstream candidate.
- `0002-v4l2m2m-dec-bounded-wait-for-frame.patch`: the decoder waited for a
  capture buffer with no time limit (`poll` timeout -1). When the driver never
  returned one (it decodes into capture buffers, and had none free), the decode
  thread sat in `poll` for good while the player kept running without pictures.
  The wait is now 500 ms, then `EAGAIN`, with an error log line saying how many
  buffers are queued in the driver and how many the application holds; the
  caller (`jv_decoder_next` in `native/media.c`) gives up on the decoder after
  3 s without a picture, so playback stops with an error instead of hanging.
  Also fixes the `poll` retry loop reading a stale `errno` when `poll` times out.
