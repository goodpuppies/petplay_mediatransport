import { drawStreamFrame, showsWholePicture, type StreamView } from "./view";
import { attachRemoteInput } from "./input";
import { sessionInit, type SourcePose, startXr, type XrStats, xrSupported } from "./xr";
import * as Moq from "@moq/net";
const canvas = document.querySelector<HTMLCanvasElement>("#video")!;
const context = canvas.getContext("2d", { alpha: false })!;
const requestedView = new URLSearchParams(location.search).get("view");
let view: StreamView = requestedView === "left" || requestedView === "stereo"
  ? requestedView
  : "right";
const viewSelect = document.createElement("select");
viewSelect.setAttribute("aria-label", "Video view");
for (
  const [value, label] of [["right", "Single eye"], ["left", "Left eye"], [
    "stereo",
    "Stereo",
  ]]
) {
  const option = document.createElement("option");
  option.value = value;
  option.textContent = label;
  viewSelect.append(option);
}
viewSelect.value = view;
document.querySelector("header")?.append(viewSelect);
// Replaced by the source's real geometry once the catalog arrives.
let source = { width: 1920, height: 1080 };
Object.assign(canvas.style, {
  width: "auto",
  height: "auto",
  maxWidth: "100%",
  maxHeight: "calc(100dvh - 140px)",
  margin: "0 auto",
});
function updateView() {
  canvas.width = showsWholePicture(view) ? source.width : source.width / 2;
  canvas.height = source.height;
  const label = document.querySelector("p");
  if (label) {
    label.textContent = view === "full"
      ? `${source.width}\u00d7${source.height} source`
      : view === "stereo"
      ? "Left eye · Right eye"
      : "Click a controller to select it, then drag an arrow or rotation ring.";
  }
}
updateView();
viewSelect.onchange = () => {
  view = viewSelect.value as StreamView;
  updateView();
};

const status = document.querySelector("#status")!;
const stats = {
  connected: false,
  received: 0,
  decoded: 0,
  dropped: 0,
  errors: [] as string[],
  marker: [] as number[][],
  transport: "",
  codec: "",
  decoderPreference: "",
  presented: 0,
  presentationDropped: 0,
  bufferMs: 20,
  clock: {} as Record<string, number | undefined>,
  sourceIntervals: [] as number[],
  arrivalIntervals: [] as number[],
  presentationIntervals: [] as number[],
  presentationLateness: [] as number[],
  xr: {
    latencyMs: [],
    warpDegrees: [],
    posed: 0,
    unposed: 0,
    presenterErrors: [],
  } as XrStats,
  poseAgeUs: [] as number[],
  /**
   * XR picture pacing. "steady" (default) shows the next picture in order every display frame
   * and drops one only after being a picture behind for `xrCatchUpFrames` frames in a row.
   * "queue" keeps at most `xrQueue` decoded pictures waiting and drops the rest at once; with
   * evenly arriving pictures it skips and repeats whenever decoding ends near the deadline
   * (42% of frames measured at 3360×1760, against 1.5% steady).
   */
  xrPacing: new URLSearchParams(location.search).get("xrPacing") ?? "steady",
  xrCatchUpFrames: 36,
  xrCatchUps: 0,
  /** Decoded pictures XR lets wait behind the one shown (0–3), with `xrPacing=queue`. */
  xrQueue: Number(new URLSearchParams(location.search).get("xrQueue") ?? 1),
  xrWaiting: [] as number[],
  /** Browser-side latency of each picture shown in XR, in milliseconds. */
  xrReceiveToDecoded: [] as number[],
  xrDecodedToShown: [] as number[],
};
// When each picture (by timestamp) arrived and finished decoding, for the XR breakdown.
const arrivedAt = new Map<number, number>(), decodedAt = new Map<number, number>();
// Per-picture receive and decode times (performance.now ms by picture timestamp), for pacing
// analysis from DevTools.
(window as any).petplayPictureTimes = { arrivedAt, decodedAt };
const remember = (map: Map<number, number>, key: number, value: number) => {
  map.set(key, value);
  if (map.size > 120) map.delete(map.keys().next().value!);
};
(window as any).petplayStream = stats;
const measure = new URLSearchParams(location.search).has("measure");
const small = document.createElement("canvas");
small.width = 480;
small.height = 270;
const analysis = small.getContext("2d", { willReadFrequently: true })!;
async function main() {
  if (
    typeof VideoDecoder === "undefined" || typeof WebTransport === "undefined"
  ) throw Error("This browser needs WebCodecs and WebTransport support.");
  const config = await (await fetch("/config.json")).json();
  const origin = new Moq.Origin.Producer();
  const url = new URL(config.url);
  // `?media=HOST` sends the media connection somewhere other than the page's host, such as
  // the source's LAN address while the page itself comes through a tailnet HTTPS proxy. The
  // certificate is pinned by hash, so an IP literal works.
  url.hostname = new URLSearchParams(location.search).get("media") ?? location.hostname;
  const connection = await Moq.Connection.connect({
    url,
    consume: origin,
    websocket: { enabled: false },
    webtransport: { serverCertificateHashes: [{ value: config.fingerprint }] },
    signal: AbortSignal.timeout(10000),
  });
  const inputAddress = new URLSearchParams(location.search).get("input");
  const closeInput = inputAddress
    ? attachRemoteInput(canvas, inputAddress, () => view)
    : undefined;
  stats.connected = true;
  stats.transport = connection.transport;
  status.textContent = "Connected · waiting for video";
  const request = origin.request(Moq.Path.from("petplay"));
  let active = request.active.peek();
  while (!active) {
    await request.active.changed();
    active = request.active.peek();
  }
  const catalog = active.track("config").subscribe().ordered();
  const meta = await catalog.readJson() as any;
  catalog.close();
  const even = (n: unknown) =>
    typeof n === "number" && Number.isInteger(n) && n >= 16 && n <= 8192 &&
    n % 2 === 0;
  if (
    meta?.version !== 1 || meta.format !== "annexb" || !even(meta.width) ||
    !even(meta.height)
  ) throw Error("Unsupported video configuration");
  // A single-view source has no eye halves to choose between.
  const sideBySide = meta.stereo === "side-by-side";
  source = { width: meta.width, height: meta.height };
  if (!sideBySide) {
    view = "full";
    viewSelect.remove();
  }
  updateView();
  const options = new URLSearchParams(location.search);
  const immediate = options.has("immediate");
  const decoderConfig: VideoDecoderConfig = {
    codec: meta.codec,
    codedWidth: meta.width,
    codedHeight: meta.height,
    optimizeForLatency: true,
    hardwareAcceleration: options.has("software")
      ? "prefer-software"
      : "prefer-hardware",
  };
  let supported = await VideoDecoder.isConfigSupported(decoderConfig);
  if (!supported.supported) {
    decoderConfig.hardwareAcceleration = "no-preference";
    supported = await VideoDecoder.isConfigSupported(decoderConfig);
  }
  if (!supported.supported) throw Error(`Unsupported codec ${meta.codec}`);
  stats.codec = meta.codec;
  stats.decoderPreference = decoderConfig.hardwareAcceleration!;
  // Source timestamps are relative microseconds; map them onto the browser clock.
  // Keep a small jitter cushion without accumulating latency when the tab stalls.
  const requestedBuffer = Number(
    new URLSearchParams(location.search).get("buffer") ?? 20,
  );
  stats.bufferMs = Number.isFinite(requestedBuffer)
    ? Math.max(0, Math.min(100, requestedBuffer))
    : 20;
  const pending: VideoFrame[] = [];
  let clockSource: number | undefined,
    clockLocal = 0,
    lastArrival: number | undefined,
    lastSource: number | undefined,
    lastPresent: number | undefined;
  let animation = 0, stopped = false;
  let lastReceivedAt = performance.now(), lastDecodedAt = performance.now();
  let recoveries = 0;
  const diagnostics = setInterval(() => {
    const quiet = performance.now() - lastReceivedAt > 1500;
    status.textContent = `${
      quiet ? "Waiting for data" : "Live"
    } · received ${stats.received} · decoded ${stats.decoded} · shown ${stats.presented} · ${
      immediate ? "direct" : `${stats.bufferMs} ms buffer`
    }${recoveries ? ` · decoder recoveries ${recoveries}` : ""}${
      stats.errors.length ? ` · ${stats.errors.at(-1)}` : ""
    }`;
    // A stalled hardware decoder must release its surfaces before the next keyframe.
    if (
      !quiet && performance.now() - lastDecodedAt > 2000 && !waitingForKey &&
      decoder.state === "configured"
    ) {
      decoder.reset();
      decoder.configure(decoderConfig);
      clearPending();
      waitingForKey = true;
      recoveries++;
      lastDecodedAt = performance.now();
    }
  }, 1000);
  function sample(values: number[], value: number) {
    values.push(value);
    if (values.length > 600) values.shift();
  }
  function clearPending() {
    for (const frame of pending) frame.close();
    pending.length = 0;
    clockSource = undefined;
  }
  // While a WebXR session runs, its frame loop takes pictures instead of the page canvas.
  let xrActive = false;
  /** Consecutive XR frames with at least one more decoded picture waiting. */
  let behindFrames = 0;
  /**
   * The picture to show now. The page canvas jumps to the newest due picture on a buffered
   * clock. XR ignores that buffer and shows decoded pictures in order, one per display frame
   * (see `stats.xrPacing`). Always jumping to the newest made moving things judder: the next
   * picture often finishes decoding right at the display deadline, so it alternately made it
   * (a skip) and then left the next frame empty (a repeat). Steady pacing instead only catches
   * up after being behind for a while, which a source slightly faster than the headset (PC
   * 72.00 Hz, Quest ~71.83 Hz) needs about once every six seconds.
   */
  function takeDue(now: number, inOrder = false): VideoFrame | undefined {
    stats.clock = {
      now,
      local: clockLocal,
      source: clockSource,
      head: pending[0]?.timestamp,
      length: pending.length,
    };
    let ready: VideoFrame | undefined;
    if (inOrder) {
      // Live-tunable from DevTools: window.petplayStream.xrPacing / xrCatchUpFrames / xrQueue.
      if (stats.xrPacing === "steady") {
        behindFrames = pending.length >= 2 ? behindFrames + 1 : 0;
        const limit = Math.max(1, Math.floor(stats.xrCatchUpFrames));
        // A stall that left several pictures waiting is cleared at once.
        while (pending.length > 3 || (behindFrames >= limit && pending.length >= 2)) {
          pending.shift()!.close();
          stats.presentationDropped++;
          stats.xrCatchUps++;
          behindFrames = 0;
        }
      } else {
        const keep = Math.max(0, Math.min(3, Math.floor(stats.xrQueue)));
        while (pending.length > keep + 1) {
          pending.shift()!.close();
          stats.presentationDropped++;
        }
      }
      ready = pending.shift();
      sample(stats.xrWaiting, pending.length);
      if (ready) {
        const arrived = arrivedAt.get(ready.timestamp), decoded = decodedAt.get(ready.timestamp);
        if (arrived !== undefined && decoded !== undefined) {
          sample(stats.xrReceiveToDecoded, decoded - arrived);
          sample(stats.xrDecodedToShown, now - decoded);
        }
      }
    }
    while (
      !inOrder && pending.length && clockSource !== undefined &&
      clockLocal + (pending[0].timestamp - clockSource) / 1000 <= now
    ) {
      if (ready) {
        ready.close();
        stats.presentationDropped++;
      }
      ready = pending.shift();
    }
    if (ready && clockSource !== undefined) {
      sample(
        stats.presentationLateness,
        now - (clockLocal + (ready.timestamp - clockSource!) / 1000),
      );
      if (lastPresent !== undefined) {
        sample(stats.presentationIntervals, now - lastPresent);
      }
      lastPresent = now;
      stats.presented++;
    }
    return ready;
  }
  function present(now: number) {
    if (stopped) return;
    const ready = xrActive ? undefined : takeDue(now);
    if (ready) {
      try {
        drawStreamFrame(context, ready, canvas, view);
        if (measure && sideBySide && stats.presented % 15 === 0) {
          analysis.drawImage(canvas, 0, 0, 480, 270);
          const p = analysis.getImageData(0, 0, 480, 270).data;
          const count = [0, 0], sum = [0, 0];
          for (let y = 0; y < 270; y++) {
            for (let x = 0; x < 480; x++) {
              const i = (y * 480 + x) * 4;
              if (p[i] > 180 && p[i + 1] < 70 && p[i + 2] < 70) {
                const e = x < 240 ? 0 : 1;
                count[e]++;
                sum[e] += x - e * 240;
              }
            }
          }
          if (count.every((n) => n > 20) && stats.marker.length < 256) {
            stats.marker.push(sum.map((n, i) => n / count[i]));
          }
        }
      } catch (error) {
        stats.errors.push(`Presentation: ${error}`);
      } finally {
        ready.close();
      }
    }
    animation = requestAnimationFrame(present);
  }
  animation = requestAnimationFrame(present);
  const decoder = new VideoDecoder({
    output(frame) {
      stats.decoded++;
      const now = performance.now();
      remember(decodedAt, frame.timestamp, now);
      lastDecodedAt = now;
      if (immediate && !xrActive) {
        try {
          drawStreamFrame(context, frame, canvas, view);
          stats.presented++;
        } catch (error) {
          stats.errors.push(`Presentation: ${error}`);
        } finally {
          frame.close();
        }
        return;
      }
      // Re-anchor after stalls or timestamp discontinuities. Cached-GOP
      // catch-up adjusts the clock separately below.
      if (
        clockSource === undefined ||
        now - (clockLocal + (frame.timestamp - clockSource) / 1000) > 150 ||
        frame.timestamp < clockSource
      ) {
        clearPending();
        clockSource = frame.timestamp;
        clockLocal = now + stats.bufferMs;
      }
      // The earliest decoded arrival bounds the clock offset. During cached-GOP
      // catch-up, move deadlines earlier so frames cannot stay permanently ahead.
      clockLocal = Math.min(
        clockLocal,
        now + stats.bufferMs - (frame.timestamp - clockSource!) / 1000,
      );
      pending.push(frame);
      if (pending.length > 12) {
        pending.shift()!.close();
        stats.presentationDropped++;
      }
    },
    error(error) {
      stats.errors.push(String(error));
      status.textContent = String(error);
    },
  });
  decoder.configure(decoderConfig);
  let waitingForKey = true;
  let endXr: (() => void) | undefined;
  // Head pose each picture was rendered for, by video timestamp (microseconds).
  const poses = new Map<number, SourcePose>();
  const poseTrack = active.track("pose").subscribe();
  (async () => {
    for (;;) {
      const group = await poseTrack.recvGroup();
      if (!group) return;
      for (;;) {
        const frame = await group.readFrame();
        if (!frame) break;
        const p = frame.payload;
        if (p.length !== 48) continue;
        const v = new DataView(p.buffer, p.byteOffset, 48);
        const f = (i: number) => v.getFloat32(i, true);
        const pose: SourcePose = {
          sequence: Number(v.getBigUint64(8, true)),
          rotation: [f(16), f(20), f(24), f(28)],
          position: [f(32), f(36), f(40)],
          ageUs: v.getUint32(44, true),
        };
        poses.set(Number(v.getBigUint64(0)), pose);
        sample(stats.poseAgeUs, pose.ageUs);
        if (poses.size > 300) poses.delete(poses.keys().next().value!);
      }
    }
  })().catch((error) => stats.errors.push(`Pose track: ${error}`));
  const keyHex = options.get("xrKey") ?? "ff00ff";
  const keyRgb = /^[0-9a-fA-F]{6}$/.test(keyHex) ? parseInt(keyHex, 16) : 0xff00ff;
  if (sideBySide) {
    const modes = ["immersive-vr", "immersive-ar"] as const;
    type Mode = typeof modes[number];
    const support = await Promise.all(modes.map((mode) => xrSupported(mode)));
    const buttons = new Map<Mode, HTMLButtonElement>();
    let activeMode: Mode | null = null;
    let starting = false;
    const label = (mode: Mode) => mode === "immersive-ar" ? "AR" : "VR";
    const updateButtons = () => {
      for (const [mode, button] of buttons) {
        button.textContent = `${activeMode === mode ? "Exit" : "Enter"} ${label(mode)}`;
        button.disabled = starting || (activeMode !== null && activeMode !== mode);
        button.title = activeMode !== null && activeMode !== mode
          ? `Exit ${label(activeMode)} first` : "";
      }
    };
    const enter = async (mode: Mode, granted?: XRSession) => {
      if (activeMode !== null || starting) {
        await granted?.end();
        return;
      }
      starting = true;
      updateButtons();
      try {
        endXr = await startXr({
          mode,
          passthroughKey: [(keyRgb >> 16 & 255) / 255, (keyRgb >> 8 & 255) / 255, (keyRgb & 255) / 255],
          takeFrame: (now) => takeDue(now, true),
          framerate: meta.framerate,
          poseFor: (timestamp) => poses.get(timestamp),
          warp: options.get("warp") !== "0",
          stats: stats.xr,
          bridge: options.get("xr") ?? undefined,
          angularVelocity: options.has("xrAngular"),
          status: (text) => status.textContent = text,
          onEnd: () => {
            xrActive = false;
            activeMode = null;
            endXr = undefined;
            starting = false;
            updateButtons();
          },
        }, granted);
        clearPending();
        xrActive = true;
        activeMode = mode;
        starting = false;
        updateButtons();
      } catch (error) {
        starting = false;
        updateButtons();
        stats.errors.push(`XR: ${error}`);
        status.textContent = `XR: ${error}`;
      }
    };
    // Offer once on page load; exiting returns to both page buttons without re-entering.
    // xrMode selects the preferred prompt; both supported modes have page buttons.
    const preferred: Mode = options.get("xrMode") === "ar" ? "immersive-ar" : "immersive-vr";
    const offer = () => {
      if (!buttons.has(preferred) || activeMode !== null || starting) return;
      navigator.xr?.offerSession?.(preferred, sessionInit)
        .then((session) => enter(preferred, session))
        .catch(() => {});
    };
    for (const [index, mode] of modes.entries()) {
      if (!support[index]) continue;
      const button = document.createElement("button");
      buttons.set(mode, button);
      button.onclick = () => {
        if (activeMode === mode) endXr?.();
        else void enter(mode);
      };
      document.querySelector("header")?.append(button);
    }
    updateButtons();
    offer();
  }
  const video = active.track("video").subscribe();
  try {
    for (;;) {
      const group = await video.recvGroup();
      if (!group) break;
      try {
        for (;;) {
          const frame = await group.readFrame();
          if (!frame) break;
          const p = frame.payload;
          if (p.length < 10 || p.length > 4 * 1024 * 1024) {
            throw Error("Invalid video frame");
          }
          stats.received++;
          const key = p[0] === 1;
          const timestamp = Number(
            new DataView(p.buffer, p.byteOffset + 1, 8).getBigUint64(0),
          );
          const arrival = performance.now();
          remember(arrivedAt, timestamp, arrival);
          lastReceivedAt = arrival;
          if (lastArrival !== undefined) {
            sample(stats.arrivalIntervals, arrival - lastArrival);
          }
          if (lastSource !== undefined) {
            sample(stats.sourceIntervals, (timestamp - lastSource) / 1000);
          }
          lastArrival = arrival;
          lastSource = timestamp;
          // Yield to decoder tasks when cached groups arrive in a burst. Resetting
          // here immediately would discard a healthy decoder's entire GOP.
          const decodeDeadline = performance.now() + 1000;
          while (
            decoder.state === "configured" && decoder.decodeQueueSize > 6 &&
            performance.now() < decodeDeadline
          ) {
            await new Promise<void>((resolve) => {
              const done = () => {
                clearTimeout(timer);
                decoder.removeEventListener("dequeue", done);
                resolve();
              };
              const timer = setTimeout(done, 100);
              decoder.addEventListener("dequeue", done, { once: true });
            });
          }
          if (decoder.state !== "configured") {
            throw Error("Video decoder stopped; try ?software=1");
          }
          if (decoder.decodeQueueSize > 6) {
            decoder.reset();
            decoder.configure(decoderConfig);
            clearPending();
            waitingForKey = true;
          }
          if (waitingForKey && !key) {
            stats.dropped++;
            continue;
          }
          waitingForKey = false;
          decoder.decode(
            new EncodedVideoChunk({
              type: key ? "key" : "delta",
              timestamp,
              data: p.subarray(9),
            }),
          );
        }
      } catch (error) {
        if (error instanceof Moq.Error.Stream) {
          decoder.reset();
          decoder.configure(decoderConfig);
          clearPending();
          waitingForKey = true;
          continue;
        }
        throw error;
      }
    }
  } finally {
    stopped = true;
    endXr?.();
    poseTrack.close();
    closeInput?.();
    clearInterval(diagnostics);
    cancelAnimationFrame(animation);
    clearPending();
    video.close();
    decoder.close();
    request.close();
    origin.close();
    connection.close();
  }
}
main().catch((error) => {
  stats.errors.push(String(error));
  status.textContent = String(error);
  console.error(error);
});
