import { drawStreamFrame, showsWholePicture, type StreamView } from "./view";
import { attachRemoteInput } from "./input";
import { startXr, xrSupported } from "./xr";
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
  url.hostname = location.hostname;
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
  function takeDue(now: number): VideoFrame | undefined {
    stats.clock = {
      now,
      local: clockLocal,
      source: clockSource,
      head: pending[0]?.timestamp,
      length: pending.length,
    };
    let ready: VideoFrame | undefined;
    while (
      pending.length && clockSource !== undefined &&
      clockLocal + (pending[0].timestamp - clockSource) / 1000 <= now
    ) {
      if (ready) {
        ready.close();
        stats.presentationDropped++;
      }
      ready = pending.shift();
    }
    if (ready) {
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
  if (sideBySide && await xrSupported()) {
    const button = document.createElement("button");
    button.textContent = "Enter VR";
    document.querySelector("header")?.append(button);
    button.onclick = async () => {
      if (endXr) return endXr();
      try {
        endXr = await startXr({
          takeFrame: takeDue,
          bridge: options.get("xr") ?? undefined,
          angularVelocity: options.has("xrAngular"),
          status: (text) => status.textContent = text,
          onEnd: () => {
            xrActive = false;
            endXr = undefined;
            button.textContent = "Enter VR";
          },
        });
        clearPending();
        xrActive = true;
        button.textContent = "Exit VR";
      } catch (error) {
        stats.errors.push(`XR: ${error}`);
        status.textContent = `XR: ${error}`;
      }
    };
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
