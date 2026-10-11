# Development host checks, 2026-09-25

These validate the diagnostic tool. **They do not verify Steam Frame.**

Host: x86_64 Nobara 44, NVIDIA GeForce RTX 4080 SUPER, FFmpeg 8.1.2.
The OpenXR loader was unavailable in this process; the inventory recorded that
error without treating it as a successful runtime test.

| Check | Result |
| --- | --- |
| Rust build with optional native FFmpeg probe | Pass |
| Four CLI integration tests | Pass |
| Clippy with warnings denied, formatting | Pass |
| H.264 720p, Vulkan, 60 frames | 60 hardware surfaces, 60 readbacks |
| HEVC Main10 720p, Vulkan, 60 frames | 60 hardware surfaces, 60 readbacks |
| AV1 Main10 720p, Vulkan, 60 frames | 60 hardware surfaces, 60 readbacks |
| Request 61 frames from a 60-frame file | Failed as required; reported 60 |
| Select llvmpipe instead of the hardware GPU | Failed as required; no software fallback |
| Invalid media, network URL, zero frame count | Rejected |

JSON evidence is in the local ignored `reports/` directory. Development headers
were extracted into `.local-deps/` from the matching distribution package; no
system libraries were installed or replaced. For this local build, set
`PKG_CONFIG_PATH="$PWD/.local-deps/sysroot/usr/lib64/pkgconfig"`.

Device discovery found `frame.local` at `192.168.68.59`. SSH was initially refused,
then became reachable after the user enabled Developer Mode. Authentication and
host trust are pending. No files have been deployed to the headset.
