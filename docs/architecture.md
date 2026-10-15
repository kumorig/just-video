# Proposed player architecture

This is a design, not a list of implemented features. Decoder choice is gated by
the tests in hardware-validation.md. Rust coordinates the app; a narrow C ABI
handles FFmpeg structures. Vulkan and OpenXR remain the rendering candidates.

## Data and timing

`SMB2/3 → bounded compressed read-ahead → FFmpeg demux → hardware decoder → GPU frame → Vulkan → OpenXR`

SMB uses the pure-Rust [`smb`](https://crates.io/crates/smb) crate (SMB2/3, signing,
encryption), chosen over libsmb2 to avoid a C cross-compile for ARM64. Connect from
the headset; no mount, root privileges or companion process on the file server.
`ReadAheadReader` (`src/readahead.rs`) exposes a blocking `Read + Seek` to FFmpeg's
custom AVIO (`native/media.c`) and keeps a window of 1 MiB reads in flight (32 MiB
by default). SMB2-only negotiation, no compression (video is incompressible) and
a 1024-credit backlog support that pipelining. Beside the window the reader
keeps a reserve: up to 1 GiB past the window, fetched only while the player
asks (it does so while paused, from the control bar), so a video can play on
without the network; fetching ahead while playing once starved playback over
Wi-Fi. FFmpeg never receives a URL or credentials, and its protocol whitelist
is empty.

Give SMB I/O, demux/decode and XR rendering independent workers. Bound compressed
read-ahead by bytes and decoded frames by surface count; start with 32 MiB and
three queued presentation frames, then tune from measurements. Preserve decoder
reference surfaces until GPU fences complete. Network blocking must never stop
head tracking or the XR frame loop. On seek, cancel old reads and invalidate all
queued packets/frames using a generation counter, then decode from a keyframe.

Decode audio locally through FFmpeg to the headset's audio system, using the
audio clock as playback master. Select video by PTS; repeat the last available
video texture while still rendering each predicted head pose. Drop late video
frames without dropping XR tracking updates. Pause holds the picture; seeking
flushes audio and video together.

The OpenXR runtime selects the Vulkan device. Use its Vulkan creation and graphics
requirements APIs; the player must not simply pick device 0 as a diagnostic might.
Avoid CPU readback and RGB intermediate buffers. Sample NV12/P010 planes with
correct matrix/range/transfer handling. Explicitly handle SDR and HDR metadata;
validate tone mapping against the actual headset swapchain/display behavior.

## Login and browsing

Minimal flow: **Shares → Folders → Play**. A saved share stores host, share, domain,
username and preferences. Its password lives in an OS credential service, never
in config, URLs, CLI arguments, logs or crash reports. Validate Secret Service
availability and unlock behavior on Frame before choosing a Rust keyring backend.
If no secure unlocked store exists, support session-only login and show **Unlock
credentials** or **Save unavailable** with an actionable detail when needed.
Do not ship encryption with a key stored beside the ciphertext.

Auto-login applies only to opted-in saved shares with an available secret. Failed
authentication stops automatic retries and offers **Sign in**. Bounded backoff
handles temporary disconnects; cancellation and offline state stay responsive.
Support forgetting a share and deleting its secret. Default to SMB2/3 and signing;
offer SMB3 encryption when supported and measure its cost on device.

Directory listing is lazy/paged where practical. Treat names as data, preserve
Unicode, reject path traversal outside the configured share, and avoid reading
whole directories into unbounded UI state. Network reads remain read-only.

## Video presentation and controls

Projection and stereo packing are independent settings:

- Projection: flat screen, equirectangular VR180, equirectangular VR360.
- Packing: mono, side-by-side, top/bottom.
- Flat view: distance, size, height, yaw/pitch; optional curvature later.
- Immersive view: rotation offset and recenter; no artificial translation of
  the captured viewpoint. Fisheye VR180 needs explicit lens calibration support
  later and must not be mislabelled as equirectangular support.

Use metadata as a suggestion with a persistent manual override. Each eye samples
its assigned rectangle with half-texel-safe bounds to prevent seam/eye bleed.
Head pose is evaluated at OpenXR predicted display time. Recenter changes the
view's reference transform, rather than resetting or disabling tracking. Handle
runtime reference-space changes and tracking loss explicitly.

OpenXR actions cover select, back, play/pause, seek, volume, recenter and view
adjustments. Bind after enumerating the actual Frame interaction profile. Show
compact in-headset controls for projection, packing and reset view.
Keep controller changes reversible and clamp screen distance/scale to valid values.

## Packaging

Ship a native ARM64 application and its userspace dependencies after checking
the device ABI. Preserve the installed graphics/OpenXR drivers. Choose a Steam
launch entry and deployment mechanism after the probe. Audit FFmpeg build options
and dependency licences before distribution. Runtime operation depends only on
the headset, network and existing SMB server.
