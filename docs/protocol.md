# UltraDisplay wire protocol v1

The tablet (client) opens one TCP connection to the host daemon (server). Display-only:
there's no input channel in v1. There are two transports:

- **USB**: plain TCP through `adb reverse tcp:47800 tcp:47800`, so the tablet connects
  to `127.0.0.1:47800`. The cable is the trust anchor, so this is also where pairing
  happens.
- **Wi-Fi**: TLS 1.3 on port 47801. The host advertises `_ultradisplay._tcp` over mDNS,
  with TXT records `id` (first 16 hex digits of the certificate fingerprint) and `proto=1`.
  The tablet only accepts the certificate whose SHA-256 it received in PAIRING. The host
  only accepts a HELLO carrying the token it issued to that `client_id`, and otherwise
  answers BYE `not paired: …`.

## Framing

Every message has an 8-byte header followed by `length` payload bytes. Integers are big-endian.

```
offset size field
0      2    magic   0x55 0x44 ("UD")
2      1    type
3      1    flags
4      4    length  (payload bytes, <= 16 MiB)
8      N    payload
```

## Messages

| type | name        | dir | payload |
|------|-------------|-----|---------|
| 0x01 | HELLO       | c→s | JSON `Hello` |
| 0x02 | CONFIG      | s→c | JSON `StreamConfig` (sent before the first VIDEO, and again whenever the stream is reconfigured) |
| 0x03 | VIDEO       | s→c | `u64 pts_us` + one Annex-B access unit. flags bit0 = keyframe |
| 0x04 | STATS       | c→s | JSON `ClientStats`, about every 500 ms |
| 0x05 | REQUEST_IDR | c→s | empty: the decoder lost sync (e.g. it was recreated) |
| 0x06 | PING        | any | `u64` opaque |
| 0x07 | PONG        | any | echo of the PING payload |
| 0x08 | BYE         | any | UTF-8 reason, then the connection closes (`busy`, `not paired: …`) |
| 0x09 | PAIRING     | s→c | JSON `Pairing`, USB only, right after HELLO |

### JSON bodies

```jsonc
// Hello
{ "protocol": 1, "device": "SM-T970",
  "width": 2800, "height": 1752,            // native panel, landscape
  "refresh_rates": [120.0, 60.0],
  "codecs": ["hevc", "h264"],                // hardware decoders available
  "client_id": "a1b2…",                      // per-install id (optional over USB)
  "token": "9f8e…" }                         // from PAIRING; required over Wi-Fi

// Pairing (USB only)
{ "host_id": "0123456789abcdef", "host_name": "my-pc",
  "fingerprint": "01234567…cdef",            // SHA-256 of the host certificate (DER)
  "lan_port": 47801, "token": "9f8e…" }

// StreamConfig
{ "codec": "hevc", "width": 2800, "height": 1752, "fps": 120, "bitrate": 80000000 }

// ClientStats
{ "fps": 119.8, "decode_ms": 4.1, "dropped": 0, "queued": 0, "rtt_ms": 1.2 }
```

## Stream rules

- The first VIDEO after a CONFIG is a keyframe. Keyframes carry VPS/SPS/PPS (HEVC) or
  SPS/PPS (H.264) in-band. Clients use them as `csd-0` when configuring the decoder.
- There are no B-frames, so decode order equals presentation order. Clients render every
  frame immediately; there's no client-side pacing.
- The host never drops encoded frames, because that would break the reference chain.
  Under backpressure it skips captured frames before encoding instead.
- Frames are sent only when the screen changes, so a static desktop produces no traffic.
- The bitrate in CONFIG is the starting value. The host lowers it on congestion and raises
  it back slowly, without a new CONFIG. Caps: 80 Mbit/s over USB, 40 Mbit/s over Wi-Fi.
- `pts_us` is the host capture time on the host's monotonic clock. It's only meaningful
  as a difference between frames.
