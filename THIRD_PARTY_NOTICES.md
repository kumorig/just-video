# Third-party notices

Just Video is MIT-licensed (see `LICENSE`). It uses the following components
under their own licenses. Full license texts are in `licenses/`.

| Component | Version | License | How it is used |
|---|---|---|---|
| [FFmpeg](https://ffmpeg.org) (libavformat, libavcodec, libavutil, libswresample) | 8.1.3 (Steam Frame build); system version on desktop | LGPL-2.1-or-later (`licenses/FFmpeg-LGPL-2.1.txt`) | Demuxing and decoding. Built without `--enable-gpl` and `--enable-nonfree`. |
| Just Video FFmpeg patch (`third_party/ffmpeg-patches/`) | none | LGPL-2.1-or-later (same as FFmpeg) | 10-bit HEVC NEON motion compensation |
| [dav1d](https://code.videolan.org/videolan/dav1d) | 1.5.4 | BSD-2-Clause (`licenses/dav1d-BSD-2-Clause.txt`) | AV1 decoding |
| [zlib](https://zlib.net) | 1.3.1 | zlib (`licenses/zlib.txt`) | Matroska content decompression |
| [smb-transport](https://github.com/afiffon/smb-rs) (vendored in `third_party/smb-transport`, one change) | 0.12.1 | MIT (`third_party/smb-transport/LICENSE`) | SMB transport |
| Rust crates (see `Cargo.lock`) | none | MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0 or combinations | Everything else |

## FFmpeg source (LGPL)

The Steam Frame build links FFmpeg statically. The exact source is FFmpeg
8.1.3 from https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz plus the patches in
`third_party/ffmpeg-patches/`, configured and built by
`scripts/build-frame-media.sh`. That script also rebuilds the libraries, so you
can relink Just Video against a modified FFmpeg with `scripts/build-frame.sh`.

## Codec patents

These licenses cover copyright only. Some video and audio formats (for example
H.264, HEVC, E-AC-3 and DTS) are covered by patents in some countries, and
decoding them may require patent licenses. This project gives no patent grant.
