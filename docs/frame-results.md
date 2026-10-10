# Steam Frame results — 2026-09-26

Device: SteamOS 0.3.0 (`vr`, build 20260922), kernel 6.18, SM8650 (Snapdragon 8 Gen 3),
Adreno 750 / Mesa Turnip 26.3-devel, system FFmpeg 7.0, glibc 2.39. Full inventory:
`reports/frame-inventory.txt` (ignored).

## Decoders

| Path | Result |
|---|---|
| Vulkan Video | **Unavailable**: Turnip exposes no `VK_KHR_video_decode_*`. |
| V4L2 `iris_decoder` (`/dev/video-dec0`) | H.264, HEVC, VP9 only (no AV1); capture NV12 8-bit only; frame sizes up to 8192×8192. |
| H.264 / HEVC 8-bit via `*_v4l2m2m` | Works, including 8K, if capture buffers are ≤ 8 (FFmpeg's default 20 → ENOMEM at 8K). |
| HEVC 10-bit via V4L2 | **Crashes iris firmware** (kernel WARN in `vidc_streamon`, firmware reload); the player never sends 10-bit to it. |
| Software (FFmpeg 7.0 CPU) | AV1 10-bit: 4K ~88 fps, 8K ~35 fps (dav1d). HEVC 10-bit: 4K 142 fps, 8K 42 fps (via just-video over SMB). |

## Streaming from SMB with the ARM64 `just-video` (cross-built, no files copied)

| Source (over SMB, direct link) | Decoder | Speed |
|---|---|---|
| 1080p H.264 | h264_v4l2m2m (hw) | 960 fps, 40× real time |
| 1280×640 HEVC 8-bit | hevc_v4l2m2m (hw) | 1688 fps |
| 4K60 HEVC 10-bit | software | 142 fps, 2.4× real time |
| 8K60 HEVC 10-bit VR180 | software | 42 fps, **0.70× real time: not playable at 60 fps** |

## Network

The headset has two Wi-Fi interfaces: `wlan0` (home network, here 2.4 GHz / 40 MHz) and
`wlanap`, a 6 GHz / 160 MHz link to the PC's Frame USB adapter (PC side `10.35.78.21`).
SMB read throughput over `wlanap`: 1.24 Gb/s with 32 × 1 MiB in flight, 377 Mb/s with 8.
Over `wlan0` reads stalled until the 10 s SMB timeout. Serve media over the direct link.

## Bundled, tuned software decoding (later on 2026-09-26)

The Frame build now statically links its own media stack (`scripts/build-frame-media.sh`):
FFmpeg 8.1.3 (minimal, `-O3`, `-mcpu=cortex-a720`), dav1d 1.5.4 and zlib 1.3.1, plus
`third_party/ffmpeg-patches/0001` (10-bit HEVC NEON motion compensation). The binary
only needs `libc`/`libm` from SteamOS.

| 8K60 HEVC Main10 over SMB | fps |
|---|---|
| SteamOS FFmpeg 7.0 | 42 |
| Bundled FFmpeg 8.1.3 | 46.5 |
| + 10-bit pel NEON patch | 47–48 (0.8× real time) |

Thread count (6–16), frame vs. frame+slice threading, big-core pinning, `skip_loop_filter`
(up to `all`), `flags2=fast` and THP via `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` each
changed the result by less than ±4%. `perf stat` shows 77–82% backend-stalled cycles and
~3.3 G LLC misses per 15 s. At 8K 10-bit, motion compensation is **memory-latency
bound** (~100 MB reference frames). Faster arithmetic does not help. The CPU runs
at ~85 °C during this load.

The NEON patch pays off when data fits in cache. 4K60 HEVC Main10 went from 142 to
**178 fps (3.0× real time)**. Hardware paths are unchanged: H.264 1080p runs at 37× and
HEVC 8-bit at 75× real time.

Conclusion: 8K60 10-bit HEVC is not playable on the Frame CPU. It needs the iris
hardware decoder to gain 10-bit support (SteamOS/kernel update). Every other format we
tested plays.

## 10-bit outlook check — 2026-10-07

Still SteamOS 0.3.0, kernel 6.18.0-gfbdbca41fd45, firmware `vpu33_4v.mbn`; iris still lists
only 8-bit capture formats (Q08C, NV12, NV21, AB24, QC24).

| Test | Result |
|---|---|
| 8K60 HEVC **8-bit**, synthetic 80 Mb/s clip, `hevc_v4l2m2m`, 8 capture buffers | **84 fps, 1.4× real time** (same with 4 buffers). The hardware is not the limit for 8K60; only 10-bit output is missing. |

Findings:
- Mainline Linux 7.2 (2026-08-16) added 10-bit (P010 / QC10C) decode to the iris driver,
  tested by Qualcomm on SM8550 and SM8650. Turnip still has no Vulkan Video and Adreno has
  no video block, so that path stays closed.
- The Frame's `iris.ko` sits at `drivers/media/platform/qcom/vcodec/iris/` and uses
  Qualcomm downstream symbols (`is_10bit_colorformat`, `vdec_set_bit_depth`), not the
  mainline driver. The mainline 10-bit series does not apply to it.
- Valve's Frame hotfix package repos (0.3.x and 0.4.x) already carry a `linux-72-deckard`
  7.2.0 kernel (built 2026-10-01) next to 6.16/6.17/6.18. Its iris module is the same
  downstream driver (one tracepoint differs), so a kernel bump alone will not enable 10-bit.
- Open SteamOS issues on the Frame's iris (VP9 firmware crash #2865, missing
  DYN_RESOLUTION #2869) had no Valve reply yet.

Conclusion unchanged: 8K60 HEVC Main10 waits on Valve/Qualcomm enabling 10-bit capture
formats in the Frame's iris driver and firmware. When a SteamOS update lands, check
`uname -r` and `v4l2-ctl -d /dev/video-dec0 --list-formats` for `P010`.

## Seek stalls and B-frame stutter — 2026-10-10

Same SteamOS 0.3.0 / kernel 6.18 / `vpu33_4v.mbn`. Test file: FC2-PPV-4808888.mp4, 1080p
29.97 H.264 High with 2 B-frames, 6 Mb/s, moov first, keyframes every 8.3 s, over SMB on
the direct link. Measured with the hidden `just-video play-probe` command (`--raw` prints
the pictures a decoder returns around a seek with a luma signature, so two decoders can be
compared picture by picture).

| Finding | Detail |
|---|---|
| FFmpeg's `*_v4l2m2m` decoders have no flush | `avcodec_flush_buffers` leaves whatever the driver holds (up to 16 queued packets plus the reference frames); those pictures come out first after a seek. The player took the first one as the landing point, so a backward seek decoded and dropped everything up to the old position: 5–10 s of "searching". Forward seeks only looked fine because the stale pictures were behind the target. |
| iris returns timestamps as given, never flags keyframes | The file's pts equal its dts (no composition offsets), so in display order they run 8.275, 8.375, 8.342, 8.408, 8.308… FFmpeg's software decoder falls back to a dts-based best-effort time (consecutive, two frames late); the V4L2 path has no dts, so the pictures showed out of step. `V4L2_BUF_FLAG_KEYFRAME` is never set on capture buffers, and negative times (edit-list leading frames) come back as 4294.9 s (2^32 µs). |

Fix (`native/media.c`, V4L2 wrapper only): each picture takes the earliest time among the
packets sent and not yet matched (pictures come out in display order, one per packet),
and the pictures still inside the decoder at a seek are counted and dropped. Verified with
the signatures: the hardware pictures match the software decoder's one for one. Seeks now
land in 40–130 ms in both directions (210 ms for a short jump forward that decodes up to
the target by design), and the file plays at a steady 29.97 fps.

The read-ahead window also restarts at 4 blocks after a jump and grows by 2 per block read
in sequence: the old 32-block burst sat on the link ahead of the next seek's reads
(475 ms → 97 ms for a seek right after another).

Still open: a proper V4L2 flush (`VIDIOC_STREAMOFF`/`STREAMON` on both queues) would skip
decoding the stale pictures (~20 at 8K ≈ 300 ms per seek) and needs FFmpeg's buffer
bookkeeping patched; and the decoder cannot be restarted after draining at end of stream.
