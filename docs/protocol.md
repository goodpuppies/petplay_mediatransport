# Prototype media contracts

These version-one contracts describe the implemented PetPlay video path. They are not yet a
stable cross-platform media API.

## Local GPU producer

Transport: Linux AF_UNIX / SOCK_SEQPACKET with SCM_RIGHTS. Each message is exactly 64 bytes,
little-endian. Ancillary descriptors are owned by the receiver after receipt.

| Offset | Type | Value |
| --- | --- | --- |
| 0 | u32 | Magic `0x50504d31` |
| 4 | u32 | Version 1 |
| 8 | u32 | Slot 0–2 |
| 12 | u32 | Flags: 0 means synchronization FD attached; 1 means already signaled |
| 16 | u64 | Producer sequence, increasing per slot |
| 24 | u64 | Monotonic capture timestamp, nanoseconds |
| 32 | u32 | Width, even, 16–8192 |
| 36 | u32 | Height, even, 16–8192 |
| 40 | u32 | Plane pitch in bytes |
| 44 | u32 | Plane offset in bytes |
| 48 | u64 | DRM modifier, currently linear (0) |
| 56 | u32 | Vulkan format, currently B8G8R8A8_UNORM (44) |
| 60 | u32 | Reserved; producer writes zero |

SCM_RIGHTS contains the DMA-BUF FD first, then the ready SYNC_FD when flags are zero.
The encoder waits for readiness before importing the image as DRM ARGB8888. No raw pixel
mapping or software conversion fallback is enabled.

The first descriptor fixes the session's geometry; a later change ends the session, because
the encoded catalog a receiver has already read describes the first one. A producer that
cannot allocate GPU memory itself may copy into a linear GBM buffer, which is both
CPU-writable and DMA-BUF exportable, and send that; the encoder cannot tell the difference
and still never reads back. `screen-streamer --media-socket` does exactly this.

The ACK is exactly 16 bytes: magic u32, slot u32, sequence u64, little-endian. It is returned
after encoding, publication, and optional recording complete. The producer also checks its GPU
fence before reusing a slot. A full pool drops capture attempts rather than blocking compositor
presentation. Malformed messages or unsupported geometry terminate the worker session.

## Encoded MoQ broadcast

Broadcast path: `petplay`. The `config` track contains one JSON frame:

```json
{"version":1,"codec":"avc1.4D0033","width":1920,"height":1080,"framerate":75,"stereo":"side-by-side","timestamp":"microseconds","format":"annexb"}
```

The codec string is derived from the actual encoder SPS; the value above is an example.
`width` and `height` are the producer's actual geometry. `stereo` is `side-by-side` for a
stereo source and `none` for a single-view source such as a desktop; the worker's `--layout`
option decides it, since a frame descriptor does not describe how many views it carries.
`framerate` is the worker's `--fps`, which also sets the keyframe interval.
The `video` track uses microsecond MoQ timestamps and creates one group per keyframe. The
payload of each frame is:

- One byte: 1 for keyframe, 0 for delta.
- Eight bytes: unsigned big-endian capture timestamp in microseconds, relative to the source's
  first published frame.
- Remaining bytes: one Annex-B H.264 access unit, with SPS/PPS carried in band.

The producer advertises a one-second retention window. The gateway forwards encoded tracks
without transcoding. Receivers must resume decoding at a keyframe after losing reference data.
