import { drawStreamFrame, showsWholePicture, type StreamView } from "./view";
import { attachRemoteInput } from "./input";
import { sessionInit, type SourcePose, startXr, type XrStats, xrSupported } from "./xr";
import { connectReceiver, createReceiverStats } from "./receiver";
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
  ...createReceiverStats(new URLSearchParams(location.search)),
  marker: [] as number[][],
  xr: {
    latencyMs: [],
    warpDegrees: [],
    posed: 0,
    unposed: 0,
    presenterErrors: [],
  } as XrStats,
  poseAgeUs: [] as number[],
};
(window as any).petplayStream = stats;
const measure = new URLSearchParams(location.search).has("measure");
const small = document.createElement("canvas");
small.width = 480;
small.height = 270;
const analysis = small.getContext("2d", { willReadFrequently: true })!;
async function main() {
  const config = await (await fetch("/config.json")).json();
  const url = new URL(config.url);
  // `?media=HOST` sends the media connection somewhere other than the page's host, such as
  // the source's LAN address while the page itself comes through a tailnet HTTPS proxy. The
  // certificate is pinned by hash, so an IP literal works.
  url.hostname = new URLSearchParams(location.search).get("media") ?? location.hostname;
  const options = new URLSearchParams(location.search);
  const immediate = options.has("immediate");
  // While a WebXR session runs, its frame loop takes pictures instead of the page canvas.
  let xrActive = false;
  const receiver = await connectReceiver({
    url,
    fingerprint: config.fingerprint,
    stats,
    bufferMs: Number(options.get("buffer") ?? 20),
    software: options.has("software"),
    direct: () => immediate && !xrActive ? (frame) => drawStreamFrame(context, frame, canvas, view) : undefined,
    onStatus: (text) => status.textContent = text,
  });
  // Per-picture receive and decode times (performance.now ms by picture timestamp), for pacing
  // analysis from DevTools.
  (window as any).petplayPictureTimes = receiver.times;
  const { meta, takeDue, clearPending } = receiver;
  const inputAddress = new URLSearchParams(location.search).get("input");
  const closeInput = inputAddress
    ? attachRemoteInput(canvas, inputAddress, () => view)
    : undefined;
  // A single-view source has no eye halves to choose between.
  const sideBySide = meta.stereo === "side-by-side";
  source = { width: meta.width, height: meta.height };
  if (!sideBySide) {
    view = "full";
    viewSelect.remove();
  }
  updateView();
  let animation = 0, stopped = false;
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
  let endXr: (() => void) | undefined;
  // Head pose each picture was rendered for, by video timestamp (microseconds).
  const poses = new Map<number, SourcePose>();
  const poseTrack = receiver.broadcast.track("pose").subscribe();
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
        stats.poseAgeUs.push(pose.ageUs);
        if (stats.poseAgeUs.length > 600) stats.poseAgeUs.shift();
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
  try {
    await receiver.run();
  } finally {
    stopped = true;
    endXr?.();
    poseTrack.close();
    closeInput?.();
    cancelAnimationFrame(animation);
  }
}
main().catch((error) => {
  stats.errors.push(String(error));
  status.textContent = String(error);
  console.error(error);
});
