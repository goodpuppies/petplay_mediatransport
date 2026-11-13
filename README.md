# petplay_mediatransport

Move a media feed from a producer to another process, peer, or receiver.

The intended scope includes sending, receiving, forwarding, and the platform-specific adapters
needed to connect media sources and destinations. PetPlay is the first consumer. OpenVR device
and compositor behavior belongs to `petplay_ovrdriver`; media transport belongs here.

## Current implementation

This is an early Linux video prototype, not yet a cross-platform media framework:

- Import Vulkan DMA-BUF frames and synchronization FDs from a Unix seqpacket socket.
- Convert RGB to NV12 and encode H.264 with VAAPI, without a CPU raw-frame fallback.
- Publish timestamped encoded video over MoQ, optionally through a loopback-only Iroh endpoint.
- Relay MoQ tracks from Iroh to browser WebTransport without decoding or re-encoding.
- Receive and decode in a WebCodecs browser viewer, with timestamp-based presentation.
- Optionally record encoded H.264 and a frame journal.

The current feed is fixed at 1920×1080 side-by-side stereo, 75 fps, and 12 Mbps. It uses
`/dev/dri/renderD128` and a three-slot producer pool. Audio, arbitrary geometry, other GPU APIs,
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

Each WebTransport listener also serves viewer HTML, JavaScript, and `config.json` over TCP on
the same IP at its port plus one. For this example the gateway viewer is
`http://127.0.0.1:4471/`. Remote browsers need an HTTPS page, access to the gateway's UDP port,
and WebTransport/WebCodecs support. The viewer pins the generated WebTransport certificate
using `config.json`; it replaces the configured hostname with the page hostname.

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

- `src/main.rs`: Linux DMA-BUF ingestion, synchronization, VAAPI encoding, recording, ACKs.
- `src/publish.rs`: custom MoQ tracks and native transport endpoints.
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
