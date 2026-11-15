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


## Native receiver output

The Linux `receive` command is a producer of the same 64-byte PPM1 descriptor and consumes the
same 16-byte ACK. Differences from capture ingress:

- Flags are always 1, with exactly one DMA-BUF FD; GPU color conversion has completed before
  transfer, so there is no extra synchronization FD.
- The image is linear DRM ARGB8888 / Vulkan B8G8R8A8_UNORM, offset zero, one plane. Source
  orientation is top-down. The initial dimensions are fixed for the session.
- The timestamp is the source's stream-relative microsecond timestamp multiplied by 1000,
  expressed in nanoseconds. It is not a synchronized receiver wall clock.
- There are three reusable slots. A slot remains unavailable until an exact slot/sequence ACK
  arrives. Unknown, duplicate, stale, or incorrectly sized ACKs terminate the receiver.
- A full output pool drops presentation frames while continuing to decode reference frames.
  A full encoded-input queue skips through the next keyframe and resets decoder references.

A consumer may ACK a frame it never imported immediately. After importing one, it must finish
all GPU reads before ACKing; closing its FD alone does not authorize buffer reuse. PetPlay
imports through EGL, blits into its own GL texture, and calls `glFinish` before ACKing. This
separates reusable transport storage from the texture OpenVR consumes asynchronously.

Only version-1 mono Annex-B H.264 catalogs are accepted. Catalog and decoded dimensions must
agree; resizing requires a fresh session. Consumer disconnect closes the receiver and removes
its socket, including when initialization fails after binding. Network errors terminate the
session rather than reconnecting silently.


## Windows native receiver output

`receive SHARE_CODE --stdio` emits newline-delimited `WINDOWS_MEDIA_FRAME ` followed by JSON:

```json
{"version":1,"handle":"1073742786","width":640,"height":480,"sequence":1,"timestampUs":0}
```

Other stdout lines are diagnostics; stderr carries failures. The decimal-string handle is a
legacy DXGI shared handle from `IDXGIResource::GetSharedHandle`, not an NT handle to duplicate
or close. It identifies one persistent BGRA8 texture owned by the receiver. The handle and
geometry are fixed for the session, including decoder reference resets. There are no CPU
pixel payloads or frame FDs. The consumer must use the same GPU adapter and hold a local pipe
connection to the native owner. There is no media authentication on this local metadata pipe;
PetPlay owns both endpoints via its child process.

The private conversion texture is updated by `VideoProcessorBlt`; only atomic `CopyResource`
updates touch the shared presentation texture, following OpenVR's DXGISharedHandle contract.
A GPU event query completes before the frame record is written. The consumer can drop stale
metadata and present the newest frame, since the texture always contains the newest completed
copy (or a later atomic update). No per-frame ACK is required for this persistent storage.

Send the exact line `STOP` on stdin or close stdin to stop. The owner retains the texture until
shutdown. PetPlay clears/destroys its OpenVR overlay before sending STOP. Failure, network
termination or forced process exit ends the session; consumers must clear their presentation
reference and start a new session rather than reuse the old numeric handle.
