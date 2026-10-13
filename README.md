# UltraDisplay

Use any Android 11+ tablet with a hardware HEVC/H.264 decoder as a
real extended **120 Hz** monitor for Arch Linux running niri.

```
niri virtual output UD-1 ─▶ ScreenCast/PipeWire (DMA-BUF) ─▶ CUDA ─▶ NVENC HEVC
   ─▶ TCP (USB via adb reverse, or Wi-Fi with TLS) ─▶ MediaCodec ─▶ 120 Hz panel
```

It needs a niri build with virtual-output support (PR #3800), packaged in
`packaging/niri-virtual/`.

## Layout

```
host/            Rust workspace
  crates/ud-capture   niri ScreenCast D-Bus + PipeWire consumer
  crates/ud-encode    direct NVENC encoder (runtime-loaded)
  crates/ud-niri      niri IPC: create/remove/configure the virtual output
  crates/ud-proto     wire protocol (docs/protocol.md)
  crates/ud-host      host runtime (listeners, pairing, sessions, events, config)
  ultradisplay/       GUI app (GTK4/libadwaita, tray, notifications)
  ultradisplayd/      headless daemon
android/         Kotlin app (plain Activity + MediaCodec, no AndroidX)
packaging/       niri-virtual PKGBUILD, AppImage assets, example config
docs/            protocol, phase results
scripts/         env.sh (toolchains/caches in .toolchain/), build-appimage.sh
```

## Build

System packages: `rust`, `clang`, `ffnvcodec-headers`, `jdk21-openjdk`, `android-tools`,
`pipewire`, and the NVIDIA driver.

```sh
. scripts/env.sh                         # Android SDK, Gradle, cargo caches under .toolchain/
(cd host && cargo build --release)
(cd android && ./gradlew assembleDebug)
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
```

## Patched niri, side by side with stock

```sh
sudo pacman -U build/makepkg/out/niri-virtual-26.04.r190.f18f277-2-x86_64.pkg.tar.zst
```

Log out, then pick **"Niri (virtual outputs)"** in the login manager.
To uninstall run `sudo pacman -R niri-virtual`.

## The app (inside the "Niri (virtual outputs)" session)

```sh
scripts/build-appimage.sh                 # -> build/appimage/UltraDisplay-x86_64.AppImage
./UltraDisplay-x86_64.AppImage            # opens the window; lives in the tray
```

The window has four pages:
- **Status**: live tablet state, resolution, fps, bitrate, encode/decode times.
- **Settings**: every option, including bitrate sliders.
- **Tablets**: paired tablets, with Forget.
- **Log**: the live log.

Closing the window keeps UltraDisplay running in the tray. **Right-click the tray icon →
Quit** (or the window menu) to stop streaming and exit. Turn on **Settings → App → Start
with the session** to launch it in the tray whenever you log into "Niri (virtual outputs)".
This writes `~/.config/systemd/user/ultradisplayd.service`, which runs the AppImage from
its current path with `--hidden`.

Notifications: tablet connected/disconnected and errors (on by default), bitrate lowered
(off by default).

Headless (no GUI, e.g. over SSH): `./UltraDisplay-x86_64.AppImage headless`
(`headless install-service` / `uninstall-service` for a systemd unit). Logs:
`journalctl --user -u ultradisplayd -f`.

GTK4 and libadwaita come from the system (`gtk4`, `libadwaita`), as do PipeWire and the
NVIDIA driver.

Plug in the tablet (USB debugging authorized). The daemon sets up `adb reverse` as soon
as the device appears. Open the UltraDisplay app on the tablet and the daemon creates
`UD-1` with the tablet's native mode and starts streaming. Closing the app, turning off
the tablet's screen, or unplugging removes `UD-1`, and niri moves its windows back
(set `keep_output = true` to keep them on UD-1 instead). Tap the tablet to toggle the
stats overlay.

On the tablet, set the display's motion smoothness to **Adaptive** (120 Hz).

### Wi-Fi

Connect the tablet over USB once. That pairs it, so the PC hands it its certificate
fingerprint and a token. After that, opening the app without the cable finds the PC on
the LAN over mDNS and connects with TLS (port 47801/tcp; mDNS uses 5353/udp). USB is
always preferred when plugged in. If your network blocks mDNS, tap **Host address…**
on the tablet's waiting screen and enter the PC's IP. **Forget pairing** clears it; pairing
again just needs another USB connection. To revoke a tablet on the PC, delete its entry in
`~/.local/share/ultradisplay/paired.toml`.

The bitrate adapts to the link: up to 80 Mbit/s over USB and 40 over Wi-Fi
(`bitrate_mbps`, `lan_bitrate_mbps`). Set `lan = false` to disable Wi-Fi entirely, or
`usb = false` to ignore the cable.

### Config

The Settings page edits `~/.config/ultradisplay/config.toml`. Bitrate, scale, position,
maximum frame rate and keep-output apply live (the frame rate switches UD-1's refresh
rate and the stream without reconnecting; it's capped by the tablet's panel). The **USB** and **Wi-Fi** switches (and ports) apply immediately:
turning one off closes it and ends a session running over it (UD-1 goes away), while a
session over the other one keeps streaming. Turning USB off also removes the `adb reverse`
rule from the tablet. Only a new output name needs Restart. Every key is optional,
and the headless daemon's CLI flags override it. Saving from the app rewrites the file
without hand-written comments. See
`packaging/ultradisplay/config.example.toml` (also inside the AppImage under `usr/share/doc/`). Most useful: `scale` (2.0 = 1400x876
logical, 1.5 = 1867x1168, close to a 1080p monitor's width), `placement`, and
`bitrate_mbps`. Restart the daemon after editing (`systemctl --user restart ultradisplayd` for the service).

niri sizes columns in logical pixels, so a fixed-width window looks wider on the tablet
at scale 2. A lower `scale` gives the tablet more logical space.

**Position** (`placement`): `auto` lets niri put the tablet right of your other monitors;
`left`, `right`, `above` and `below` place it next to the monitor at (0, 0) (top edges
aligned for left/right, left edges for above/below) and follow scale changes; `manual` uses
`position = [x, y]`. A config with only `position` (from older versions) counts as manual.
Everything is applied over niri IPC; nothing in `~/.config/niri/` is changed.

## Tablet app builds

```sh
(cd android && ./gradlew assembleRelease)    # signed, shrunk: app/build/outputs/apk/release/app-release.apk
(cd android && ./gradlew assembleDebug)      # debug build
```

The release key lives in `.toolchain/signing/` (`release.jks` + `keystore.properties`,
generated once, never committed). **Back it up.** Android only installs an update signed
with the same key. A debug build and a release build can't replace each other either:
switching means `adb uninstall com.ultradisplay.app` first, which also clears the app's
Wi-Fi pairing (the next USB connection re-pairs automatically).

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Wi-Fi: app never finds the PC | The network may block mDNS (common on guest/enterprise Wi-Fi). Use **Host address…** on the tablet. The PC needs 47801/tcp reachable, plus 5353/udp for discovery. |
| Wi-Fi: "pairing was rejected" | The PC's pairing data was reset or the tablet was removed from `paired.toml`. Connect over USB once. |
| Wi-Fi: "not the paired host (certificate mismatch)" | Another machine answered at that address, or the PC's key in `~/.local/share/ultradisplay/` was regenerated. Use **Forget pairing**, then connect over USB. |
| Windows look too big on the tablet | Lower `scale` in `~/.config/ultradisplay/config.toml` (1.5 ≈ a 1080p monitor's logical width). |
| Turning off the tablet's screen moves its windows back | Expected: the stream stops. `keep_output = true` keeps UD-1 (and its windows) while the tablet sleeps. |
| Vulkan apps hang on a nested/headless test niri | A broken implicit Vulkan layer (here `lsfg-vk`). Use `VK_LOADER_LAYERS_DISABLE='~implicit~'` for test clients. |
| Log says `capture: SHM` instead of `zero-copy DMA-BUF` | EGL/CUDA interop wasn't available or an import failed (see the warning above it). Streaming still works, just with more CPU. `zero_copy = false` forces SHM. |
| AppImage won't start: FUSE error | Install `fuse2`/`fuse3`, or run `./UltraDisplay-x86_64.AppImage --appimage-extract-and-run`. |
