/**
 * Receiving one PetPlay media feed in a browser: MoQ over WebTransport, the `config` catalog,
 * WebCodecs decoding with recovery, and paced presentation of decoded pictures. Pages that show
 * a feed (the stream page, PetPlay's scene viewer) use this and only decide where pictures go.
 */
import * as Moq from "@moq/net";

/** The `config` track's catalog. */
export type VideoMeta = {
  version: 1;
  format: "annexb";
  codec: string;
  width: number;
  height: number;
  stereo?: string;
  framerate?: number;
};

/** Counters and timing samples a receiver maintains; pages may extend the object. */
export type ReceiverStats = {
  connected: boolean;
  received: number;
  decoded: number;
  dropped: number;
  errors: string[];
  transport: string;
  codec: string;
  decoderPreference: string;
  presented: number;
  presentationDropped: number;
  bufferMs: number;
  clock: Record<string, number | undefined>;
  sourceIntervals: number[];
  arrivalIntervals: number[];
  presentationIntervals: number[];
  presentationLateness: number[];
  /**
   * XR picture pacing. "steady" (default) shows the next picture in order every display frame
   * and drops one only after being a picture behind for `xrCatchUpFrames` frames in a row.
   * "queue" keeps at most `xrQueue` decoded pictures waiting and drops the rest at once; with
   * evenly arriving pictures it skips and repeats whenever decoding ends near the deadline
   * (42% of frames measured at 3360×1760, against 1.5% steady).
   */
  xrPacing: string;
  xrCatchUpFrames: number;
  xrCatchUps: number;
  /** Decoded pictures XR lets wait behind the one shown (0–3), with `xrPacing=queue`. */
  xrQueue: number;
  xrWaiting: number[];
  /** Browser-side latency of each picture shown in XR, in milliseconds. */
  xrReceiveToDecoded: number[];
  xrDecodedToShown: number[];
};

/** Fresh stats; `options` reads the page's `xrPacing` and `xrQueue` query overrides. */
export function createReceiverStats(options: URLSearchParams): ReceiverStats {
  return {
    connected: false,
    received: 0,
    decoded: 0,
    dropped: 0,
    errors: [],
    transport: "",
    codec: "",
    decoderPreference: "",
    presented: 0,
    presentationDropped: 0,
    bufferMs: 20,
    clock: {},
    sourceIntervals: [],
    arrivalIntervals: [],
    presentationIntervals: [],
    presentationLateness: [],
    xrPacing: options.get("xrPacing") ?? "steady",
    xrCatchUpFrames: 36,
    xrCatchUps: 0,
    xrQueue: Number(options.get("xrQueue") ?? 1),
    xrWaiting: [],
    xrReceiveToDecoded: [],
    xrDecodedToShown: [],
  };
}

export type ReceiverOptions = {
  /** The WebTransport URL; its certificate is pinned by `fingerprint`, so an IP literal works. */
  url: URL;
  fingerprint: string;
  stats: ReceiverStats;
  /** Presentation cushion in ms (clamped to 0..100). */
  bufferMs?: number;
  /** Ask the browser for a software decoder. */
  software?: boolean;
  /**
   * Called for each decoded picture: a function returned takes it directly, skipping the paced
   * queue (`?immediate`). The receiver counts it as presented and closes it afterwards.
   */
  direct?: () => ((frame: VideoFrame) => void) | undefined;
  /** The once-a-second status line. */
  onStatus?: (text: string) => void;
  /** Bounds connection setup. */
  signal?: AbortSignal;
};

export type Receiver = Awaited<ReturnType<typeof connectReceiver>>;

function sample(values: number[], value: number) {
  values.push(value);
  if (values.length > 600) values.shift();
}

function remember(map: Map<number, number>, key: number, value: number) {
  map.set(key, value);
  if (map.size > 120) map.delete(map.keys().next().value!);
}

export async function connectReceiver(options: ReceiverOptions) {
  if (
    typeof VideoDecoder === "undefined" || typeof WebTransport === "undefined"
  ) throw Error("This browser needs WebCodecs and WebTransport support.");
  const { stats } = options;
  const origin = new Moq.Origin.Producer();
  const connection = await Moq.Connection.connect({
    url: options.url,
    consume: origin,
    websocket: { enabled: false },
    webtransport: { serverCertificateHashes: [{ value: options.fingerprint }] },
    signal: options.signal ?? AbortSignal.timeout(10000),
  });
  stats.connected = true;
  stats.transport = connection.transport;
  options.onStatus?.("Connected · waiting for video");
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
  const decoderConfig: VideoDecoderConfig = {
    codec: meta.codec,
    codedWidth: meta.width,
    codedHeight: meta.height,
    optimizeForLatency: true,
    hardwareAcceleration: options.software ? "prefer-software" : "prefer-hardware",
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
  const requestedBuffer = Number(options.bufferMs ?? 20);
  stats.bufferMs = Number.isFinite(requestedBuffer)
    ? Math.max(0, Math.min(100, requestedBuffer))
    : 20;
  // When each picture (by timestamp) arrived and finished decoding, for the XR breakdown.
  const arrivedAt = new Map<number, number>(), decodedAt = new Map<number, number>();
  const pending: VideoFrame[] = [];
  let clockSource: number | undefined,
    clockLocal = 0,
    lastArrival: number | undefined,
    lastSource: number | undefined,
    lastPresent: number | undefined;
  let lastReceivedAt = performance.now(), lastDecodedAt = performance.now();
  let recoveries = 0;
  let waitingForKey = true;
  const immediateLabel = () => options.direct?.() ? "direct" : `${stats.bufferMs} ms buffer`;
  const diagnostics = setInterval(() => {
    const quiet = performance.now() - lastReceivedAt > 1500;
    options.onStatus?.(
      `${
        quiet ? "Waiting for data" : "Live"
      } · received ${stats.received} · decoded ${stats.decoded} · shown ${stats.presented} · ${immediateLabel()}${
        recoveries ? ` · decoder recoveries ${recoveries}` : ""
      }${stats.errors.length ? ` · ${stats.errors.at(-1)}` : ""}`,
    );
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
  function clearPending() {
    for (const frame of pending) frame.close();
    pending.length = 0;
    clockSource = undefined;
  }
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
  const decoder = new VideoDecoder({
    output(frame) {
      stats.decoded++;
      const now = performance.now();
      remember(decodedAt, frame.timestamp, now);
      lastDecodedAt = now;
      const direct = options.direct?.();
      if (direct) {
        try {
          direct(frame);
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
      options.onStatus?.(String(error));
    },
  });
  decoder.configure(decoderConfig);

  /** Receives and decodes until the feed ends; then releases the decoder and connection. */
  async function run(): Promise<void> {
    const video = active!.track("video").subscribe();
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
      clearInterval(diagnostics);
      clearPending();
      video.close();
      decoder.close();
      request.close();
      origin.close();
      connection.close();
    }
  }

  return {
    meta: meta as VideoMeta,
    /** The feed's broadcast, for its other tracks (the stream page's `pose`). */
    broadcast: active,
    /** Receive and decode times by picture timestamp (performance.now ms), for pacing analysis. */
    times: { arrivedAt, decodedAt },
    takeDue,
    clearPending,
    run,
  };
}
