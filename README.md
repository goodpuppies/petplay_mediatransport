# petplay_mediatransport

Move a media feed from a producer to another process, peer, or receiver.

The intended scope includes sending, receiving, forwarding, and the platform-specific adapters
needed to connect media sources and destinations. PetPlay is the first consumer. OpenVR device
and compositor behavior belongs to `petplay_ovrdriver`; media transport belongs here.

## Current implementation

This is an early video prototype with Linux encoding and Linux/Windows native reception:

- Import Vulkan DMA-BUF frames and synchronization FDs from a Unix seqpacket socket.
- Convert RGB to NV12 and encode H.264 with VAAPI, without a CPU raw-frame fallback.
- Publish timestamped encoded video over MoQ, through a loopback-only or a peer-to-peer
  Iroh endpoint announced as a share code.
- Relay MoQ tracks from Iroh to browser WebTransport without decoding or re-encoding.
- Receive and decode in a WebCodecs browser viewer, with timestamp-based presentation.
- Decode mono H.264 on Linux VA-API into RGB DMA-BUF slots, or on Windows Media Foundation
  into a persistent shared D3D11 RGB texture.
- Optionally record encoded H.264 and a frame journal.

The feed's geometry comes from the producer's first frame descriptor; `--fps`, `--bitrate` and
`--layout mono|side-by-side` set the rest, defaulting to 75 fps, 12 Mbps and side-by-side for
the SteamVR source. It uses `/dev/dri/renderD128` and a three-slot producer pool. Audio, arbitrary geometry, other GPU APIs,
other operating systems, and general remote Iroh discovery are not implemented yet. The
current MoQ tracks are a custom protocol, not a generic Hang catalog.

## Build and check

From this repository's root, with Rust, Bun, and a working Linux VAAPI development environment:

```sh
bun install --cwd web --frozen-lockfile
bun run --cwd web check
bun run --cwd web build
cargo test --release --features iroh
cargo build --release --features iroh
```

A viewer-only build drops the GPU encoder and its DMA-BUF ingestion, which is what a
receiver on a platform without VA-API needs. It can only run the `gateway` subcommand:

```sh
cargo build --release --no-default-features --features iroh
```

The Cargo package is `petplay-mediatransport`; the executable remains `petplay-media`.
Viewer assets must be built before starting the executable. They are loaded from this checkout's
`web/` directory; the executable is not yet a standalone deployable bundle.

## Run

Start the source before its DMA-BUF producer. Use `-` to disable recording:

```sh
mkdir -p _scratch
target/release/petplay-media "$PWD/_scratch/media.sock" - 127.0.0.1:4450 --iroh-local
```

The source prints `IROH_READY id=… address=…` and listens for the producer on the socket.
`--iroh-local` explicitly disables public relay and address lookup services, binding Iroh only
to loopback. Start a gateway with those printed values:

```sh
target/release/petplay-media gateway iroh://SOURCE_ID/ SOURCE_ADDRESS 127.0.0.1:4470
```

## Peer-to-peer over the internet

`--iroh-share` replaces `--iroh-local` with an endpoint that can reach a peer across the
internet, and prints one pasteable token:

```sh
target/release/petplay-media "$PWD/_scratch/media.sock" - 127.0.0.1:4450 --iroh-share \
  --layout mono --fps 60
# IROH_SHARE code=...
target/release/petplay-media gateway CODE 127.0.0.1:4470
# GATEWAY_DIRECT peer=...
```

**A share code is a secret.** It carries the endpoint id, relay URL and direct addresses,
and nothing is published to a discovery service — so holding the code is the only way to
find the source, and also all that is needed to watch it. Restricting a source to specific
viewer identities is not implemented. The code also contains the source's IP addresses.

The gateway waits for a direct path before starting its browser listener, failing after
15 seconds if it cannot establish one. MoQ track requests are lazy, so no browser can request
video/config frames during relay-assisted setup. `GATEWAY_DIRECT` is printed before
`VIEWER_READY` and `GATEWAY_READY`.

After startup the gateway observes the direct route every 100 ms and stops when iroh reports
it lost. This is a fail-on-observed-loss policy, not a packet-level relay prohibition: iroh's
path detection and a subsequent poll can lag a path transition. Brief fallback traffic during
that detection window is still possible. Relays may carry encrypted connection setup and MoQ
announcements before media is enabled.

Each WebTransport listener also serves viewer HTML, JavaScript, and `config.json` over TCP on
the same IP at its port plus one. For this example the gateway viewer is
`http://127.0.0.1:4471/`. Remote browsers need an HTTPS page, access to the gateway's UDP port,
and WebTransport/WebCodecs support. The viewer pins the generated WebTransport certificate
using `config.json`; it replaces the configured hostname with the page hostname.

A desktop producer uses the same worker. Start it with the desktop's own geometry and layout,
then point the capture helper at the socket:

```sh
target/release/petplay-media "$PWD/_scratch/media.sock" - 127.0.0.1:4450 --iroh-local \
  --layout mono --fps 60
../screen-streamer/target/release/screen-streamer --no-input --no-frame-socket \
  --media-socket="$PWD/_scratch/media.sock"
```

Linux desktop capture opens a ScreenCast portal dialog unless a restore token already exists.
Inside PetPlay, set `PETPLAY_DESKTOP_MEDIA_SOCKET` to the worker's socket and the existing
desktop overlay publishes to it as well as to the overlay. See the parent project's
`markdown/remote-desktop-stream.md`.

For the PetPlay headless test producer, run from the parent PetPlay checkout while no real
SteamVR session is active:

```sh
python3 utils/petplay-ovrdriver-lab.py run --headless-display \
  --media-socket "$PWD/submodules/petplay_mediatransport/_scratch/media.sock" \
  --direct-server --x11-compositor --guard-monitor --seconds 30
```

The lab owns SteamVR startup, restoration, and shutdown. The native worker accepts a single
producer session and exits if it disconnects. SteamVR's current lab teardown can abort the
compositor and reset that connection; graceful shutdown is unfinished.

## Native Linux GPU receiver

Build with `cargo build --release --features decode` (or add `--no-default-features` for a
receiver without the encoder). A mono source started with `--layout mono` is required:

```sh
target/release/petplay-media receive SHARE_CODE /PRIVATE_DIRECTORY/frames.sock
# Local transport test instead of a share code:
target/release/petplay-media receive iroh://SOURCE_ID/ SOURCE_ADDRESS /PRIVATE_DIRECTORY/frames.sock
# A source's own WebTransport listener (what a browser takes), pinned to its certificate:
target/release/petplay-media receive https://HOST:PORT/ CERT_SHA256_HEX /PRIVATE_DIRECTORY/frames.sock
```

The worker binds a mode-0600 Unix seqpacket socket, prints `RECEIVER_READY`, and waits for its
local consumer before subscribing. Share codes use the same direct-route gate and route-loss
policy as the gateway. It decodes to NV12 DMA-BUFs, converts to a three-slot linear RGB GBM
pool on the GPU, and transfers descriptors rather than raw pixels. There is no CPU decoder or
pixel download fallback. `/dev/dri/renderD128` is currently fixed.

The consumer must honor [the output ACK contract](docs/protocol.md#native-receiver-output).
PetPlay's `remoteMediaOverlay` actor supplies that consumer and an EGL-backed GL texture for
OpenVR. It performs a GPU copy into presentation-owned storage before ACKing a slot. This is
zero CPU raw-frame transfer, with GPU color conversion and a presentation copy. It is not a
copy-free path. The Windows receiver uses Media Foundation and D3D11 instead, and builds independently of VA-API.

## Native Windows GPU receiver

```sh
cargo build --release --no-default-features --features decode-windows
target/release/petplay-media.exe receive SHARE_CODE --stdio
```

The inbox H.264 MFT receives a D3D11 device manager. CPU-backed output is rejected: decoded
samples must expose NV12 hardware decoder surfaces. D3D11 video processing converts to private
BGRA storage, then an atomic `CopyResource` updates one persistent legacy-shared BGRA texture.
The event query completes before a `WINDOWS_MEDIA_FRAME` JSON line publishes the handle and
geometry. Deno/OpenVR can use that handle without GL interop or receiving raw pixels.

Set `PETPLAY_DXGI_ADAPTER` to a DXGI adapter index to match the consumer; otherwise the default
hardware adapter is used. PetPlay's Windows actor selects SteamVR's adapter automatically. The
local consumer must be on the same adapter. It sends `STOP\n` on stdin, or closes stdin, to
stop; clear the overlay's texture reference before releasing the native owner.

Windows hardware decoding, cross-process shared-image access, and Linux-to-Windows Iroh
reception have been validated with synthetic imagery. Windows OpenVR submission still needs a
headset target. `decode-test FIXTURE WIDTH HEIGHT [DIAGNOSTIC.bgra] [--hold] [--reset-test]` accepts a finite
fixture of u32 little-endian lengths followed by Annex-B access units. `shared-test HANDLE
WIDTH HEIGHT DIAGNOSTIC.bgra` checks a second process can read the shared image. Only these
explicit diagnostic commands download pixels; normal reception does not. `--reset-test` replays
the fixture after flushing references and checks that the shared handle stays unchanged.

## Viewer diagnostics

The viewer shows receive, decode, and presentation counters. `window.petplayStream` contains
bounded timing samples. These measure browser processing/submission, not physical scanout or
end-to-end latency.

- `?buffer=40`: change the default 20 ms presentation cushion (clamped to 0–100 ms).
- `?immediate=1`: draw directly on decode for comparison.
- `?software=1`: request software decoding instead of preferring hardware.
- `?measure=1`: sample the stereo test marker; useful for automated validation.

Presentation uses source timestamps and requestAnimationFrame, drops superseded decoded frames,
and bounds retained VideoFrames. Cached-GOP catch-up advances the clock offset so due frames
cannot be continuously evicted before presentation. Decode submission waits for queue space;
a sustained blocked decoder is reset and waits for a keyframe. A 75 fps feed displayed at 60 Hz
necessarily skips some source frames.

## Layout and next boundaries

- `src/source.rs`: Linux DMA-BUF ingestion, synchronization, VAAPI encoding, recording, ACKs.
- `src/receive_input.rs`: native MoQ subscription and bounded encoded-input queue.
- `src/receive.rs`: Linux VA-API decode loop.
- `src/windows_decode.rs`: Windows Media Foundation and D3D11 GPU decoding/conversion.
- `src/windows_receive.rs`: Windows native receive loop and local handle metadata.
- `src/receive_output.rs`: GPU color conversion, GBM slots, descriptor transfer, exact ACKs.
- `src/publish.rs`: custom MoQ tracks and native transport endpoints.
- `src/peer.rs`: share codes, the peer endpoint, and the direct-path requirement.
- `src/gateway.rs`: Iroh-to-WebTransport relay.
- `src/web.rs`: viewer assets and certificate configuration.
- `web/`: TypeScript WebCodecs receiver and presentation.
- [docs/protocol.md](docs/protocol.md): current producer and encoded-track contracts.
- `markdown/`: design work and implementation notes.
- `_scratch/`: ignored local experiments and artifacts.

As additional feeds and platforms arrive, separate platform ingress/codec adapters from encoded
media publication and reception. Already encoded feeds should be able to forward without
re-encoding. Add those boundaries around real consumers rather than making the Linux GPU
prototype the universal interface.

## WebXR client

For a side-by-side stream, the viewer offers "Enter VR" when the browser supports
`immersive-vr` (`web/src/xr.ts`). Each eye's half is drawn at the source frustum in that eye's
view space. `?xr=wss://HOST/xr` streams head pose, grip poses and gamepad state every XR frame
to PetPlay's XR bridge, which drives the synthetic SteamVR headset and controllers. The bridge
replies with the source's FOV tangents. There is no reprojection yet. See the parent project's
`markdown/webxr-client.md`.

## Optional raw scene input

The existing stream viewer accepts an `input` query parameter containing a WebSocket URL, for
example `?input=ws%3A%2F%2F127.0.0.1%3A3991%2Finput`. This enables a thin pointer/wheel sender
on the video canvas. It reports normalized coordinates over the complete side-by-side stream,
buttons, modifiers, and pointer identities. It does not render or interpret the PetPlay scene.
PetPlay owns stereo eye selection, camera rays, pmndrs events, capture, and gizmos.

Input is currently a separate ordered WebSocket connection to PetPlay. Remote HTTPS viewers
need a reachable `wss:` endpoint, and PetPlay must allow the exact viewer origin. Omitting the
query parameter keeps the viewer receive-only. See the parent PetPlay project's
`markdown/remote-scene-controls.md` for the opt-in scene and driver configuration.

The viewer now defaults to a single right-eye view. Its view selector also offers the left eye
and stereo (`?view=left` or `?view=stereo` set the initial choice). This crops the existing
side-by-side decoded feed; the native stream remains stereo. Pointer coordinates are mapped
back into the selected source eye before sending, including during captured drags. Canvas
aspect ratio is preserved on narrow screens so CSS scaling does not offset picking.


## Windows GPU source (`capture-wgc`)

Build with `--no-default-features --features decode-windows,capture-windows` (it needs
`../screen-streamer/capture`, the `petplay-capture` crate). One monitor is captured with Windows
Graphics Capture, converted BGRA→NV12 on the D3D11 video processor and encoded by the GPU's
hardware H.264 Media Foundation encoder (NVENC, AMF or Quick Sync), then published like the Linux
encoder. No pixel is read back to the CPU and no FFmpeg is involved.

```sh
petplay-media capture-wgc --list                      # WGC_MONITOR index=… name=… width=… height=…
petplay-media capture-wgc 0 --iroh-share              # IROH_SHARE code=…, for `receive SHARE_CODE`
petplay-media capture-wgc 0 100.64.0.5:4470           # pinned WebTransport listener instead
```

Capture needs the logged-on desktop session: a process started over SSH runs in another session
and cannot capture. A still screen repeats its last picture 4 times, then once a second with a
keyframe per `--keyframe-seconds`. Checked 2026-11-21 on a GTX 1660 Ti laptop: NVIDIA H.264
Encoder MFT, received and decoded on Linux over a direct iroh path.

Cross-building from Linux works with `mingw-w64-gcc` installed:
`cargo build --release --target x86_64-pc-windows-gnu --no-default-features --features
decode-windows,capture-windows`.

## Existing Rust desktop capture on Windows

A `publish` feature enables already-encoded ingress independently of a platform encoder.
`publish-annexb WIDTH HEIGHT FPS` reads AUD-delimited Annex-B H.264 on stdin and publishes
mono tracks through a share-code endpoint. It does not decode or re-encode that input.

On Windows, `capture-tcp PORT FFMPEG_EXE FPS` accepts the existing `screen-streamer` BGRA
chunk protocol on **localhost only**, starts one NVENC encoder child, and publishes its encoded
output. Start `screen-streamer.exe --port=PORT --fps=30 --single-monitor --no-input` after
`CAPTURE_LISTEN` appears. Capture geometry is fixed for the connection. The encoder scales to
1280×720 while preserving aspect ratio. This adapter deliberately uses the existing CPU BGRA
capture path; it is not an all-GPU capture adapter. No raw pixels traverse Iroh.

Use an FFmpeg build compatible with the installed NVIDIA driver. The PetPlay prototype uses
portable 8.0.1 because the machine's NVENC API 13.0 driver cannot load the installed 8.1.2
encoder's required API 13.1. No FFmpeg capture filters or chained FFmpeg processes are needed.
The native publisher exits when capture/encoding ends. The caller owns the lifetime of the
capturer and encoder and must stop its process tree during teardown.

## Remote connections (`link`)

`petplay-media link` is a process's iroh node for stageforge remote actors, driven over stdio with
newline-delimited JSON by PetPlay (`classes/irohCarrier.ts`). Each connection is one iroh
connection with ALPN `petplay/link/1` and one bidirectional stream of length-prefixed UTF-8
frames; the dialer writes first. A process publishes only `{id, relay}` (no IP addresses) in topic
records; peers dial by id through that relay and iroh holepunches to a direct path when it can.
`--relay URL` uses a self-hosted or dev relay; `--local` is loopback only, for tests. Needs
`--features iroh`; see `src/link.rs` for the protocol.

`link-web/` is the browser side: the same ALPN and framing in a wasm-bindgen crate pinned to the
same iroh (=1.4.1). Browsers are relay-only: a page dials through the other side's relay and is
reached through its own, so it publishes `{id, relay}` and accepts like any process. Build with
`link-web/build.sh` (needs `rustup target add wasm32-unknown-unknown` and wasm-bindgen-cli
=0.2.130), which writes `pkg-web` (pages) and `pkg-deno` (tests). For a local relay, `cargo
install iroh-relay --version =1.4.1 --features server --locked --root target/tools`, then
`iroh-relay --dev`.
