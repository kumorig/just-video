## 2026-10-16
I've pulled in a few PR's with little to no testing. But many are behind feature-toggles (on the settings page).
Just a warning that things might be a bit messy until consistency/bugs has been ironed out.

```
AI Disclaimer: Text below is AI generated, sorry I'm lazy.
Most of the code is as well.
AI contributions are welcome (as to not be a hypocrite).  
```

# Just Video

A VR video player for Steam Frame that streams straight from SMB shares. (You make butter.)

## Build

On Linux or macOS with Rust, `curl`, `make`, `pkg-config` and
`python3` (or [uv](https://docs.astral.sh/uv/), used when present):

```sh
rustup target add aarch64-unknown-linux-gnu
cargo install cargo-zigbuild
bash scripts/build-frame-media.sh   # once: FFmpeg + dav1d for the Frame
bash scripts/build-frame.sh
```

## Install on Steam Frame

1. On the Frame, enable **Developer Mode** and set a user password under **Developer**.
2. From the PC, set up SSH once:
   ```sh
   ssh-keygen -t ed25519 -f ~/.ssh/steam_frame_ed25519
   ssh-copy-id -i ~/.ssh/steam_frame_ed25519 steamos@frame.local
   ```
3. Install (adds **Just Video** to the Steam library):
   ```sh
   bash scripts/install-frame.sh   # FRAME_HOST=<ip> if frame.local doesn't resolve
   ```

The same steps work from a Mac: the build cross-compiles with zig, and
`ssh-copy-id` and `rsync` ship with macOS.

## Run

Start **Just Video** from the Steam library on the Frame, choose **Add server**
and enter your SMB server's address and login. Videos on the Frame itself are
under **This headset**: Videos, Downloads, the home folder, SD card and USB drives.

## Controls

- **Trigger** or **A**: select. **B**: back, or stop the video.
- **Thumbstick**: scroll; hold the **grip** to scroll faster. **Click** it to reset the view.
- **Grip**: drag the browser or control bar, nearer or farther.
- **D-pad left/right** (or a sideways stick flick): jump 5 seconds, a minute with the grip held. **D-pad up/down**: headset volume. Lengths are in Settings.
- **Hold** a file or server to rename, move or delete it.
- **D-pad in menus** (off by default, in Settings): the D-pad moves through lists, forms and dialogs; A chooses, B goes back.
