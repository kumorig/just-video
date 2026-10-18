# Changelog

## 0.1.0 (2026-10-18)

- Changelog started.
- Fix crash when changing screen curvature (too many composition layers).
- Browser starts farther away; thumbstick moves it nearer or farther.
- Smoother browser scrolling: faster panel redraw, frame-time based motion.
- Buffered video keeps playing offline; decoder recovers once after standby.
- Buffer label shows a percentage.
- Subtitle parsing fixes (blank cues with spaces, stray braces).
- IPv6 addresses in SMB URLs.
- Hint when a password is typed into the user field.
- Settings saves keep keys from other versions.
- Each config save uses its own temporary file.
- Forward jump near the end stops 5 s short instead of ending the video.
- A file replaced by a shorter one starts from the beginning.
- "Playback stopped" dialog on read or decode errors.
- Lower memory: freed heap returned, 6 decoder buffers.
- Read-ahead window kept when the demuxer reads back.
- Faster recovery after a connection drop: failed blocks re-requested at once.
- Much lower UI memory: glyphs drawn on demand from mapped font files.
- Faster browser repaint (about 1 ms on the headset).
