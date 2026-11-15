/**
 * WebXR shell for a side-by-side stereo stream. It owns everything that must survive code
 * changes: the immersive session, WebGL context, layer, bridge socket, picture texture and the
 * pose sequence. Drawing and uplink live in the presenter (presenter.ts → /presenter.js),
 * which the shell imports at start and swaps in place whenever `/build` changes, without
 * ending the session. A presenter that throws within its first frames is replaced by the
 * previous one, so a bad build cannot black out the headset.
 */
import type {
  Presenter,
  PresenterModule,
  SourceFov,
  XrContext,
  XrOptions,
  XrShared,
} from "./xrTypes";
export type { SourcePose, XrOptions, XrStats } from "./xrTypes";

export async function xrSupported(): Promise<boolean> {
  try {
    return !!navigator.xr && await navigator.xr.isSessionSupported("immersive-vr");
  } catch {
    return false;
  }
}

/** Session init shared by the page button and the browser's own offer. */
export const sessionInit = { requiredFeatures: ["local-floor"] };

async function fetchBuild(): Promise<string> {
  const response = await fetch("/build", { cache: "no-store" });
  if (!response.ok) throw Error(`presenter build stamp: ${response.status}`);
  return (await response.text()).trim();
}
async function importPresenter(build: string): Promise<PresenterModule> {
  const url = new URL(`/presenter.js?build=${encodeURIComponent(build)}`, location.href).href;
  return await import(/* webpackIgnore: true */ url) as PresenterModule;
}

/**
 * Runs an immersive session. Pass `session` when the browser granted one (offerSession);
 * otherwise this must run inside a click handler's user activation.
 */
export async function startXr(options: XrOptions, granted?: XRSession): Promise<() => void> {
  const xr = navigator.xr;
  if (!xr) throw Error("WebXR is unavailable");
  const build = await fetchBuild();
  const firstModule = importPresenter(build);
  const session = granted ?? await xr.requestSession("immersive-vr", sessionInit);
  const canvas = document.createElement("canvas");
  const gl = canvas.getContext("webgl2", { xrCompatible: true, alpha: false, antialias: false });
  if (!gl) {
    await session.end();
    throw Error("WebGL2 is unavailable");
  }
  const layer = new XRWebGLLayer(session, gl, { antialias: false, depth: false });
  session.updateRenderState({ baseLayer: layer, depthNear: 0.05, depthFar: 100 });
  const space = await session.requestReferenceSpace("local-floor");
  // Display at the source's rate so every picture is shown for exactly one refresh.
  const rates = session.supportedFrameRates ? Array.from(session.supportedFrameRates) : [];
  if (options.framerate && rates.includes(options.framerate) && session.updateTargetFrameRate) {
    // Not awaited: Quest resolves this only once frames are being submitted, so awaiting it
    // before the frame loop starts leaves an immersive session that never draws.
    session.updateTargetFrameRate(options.framerate).catch(() => {});
  }
  const texture = gl.createTexture()!;
  gl.bindTexture(gl.TEXTURE_2D, texture);
  for (
    const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR], [
      gl.TEXTURE_WRAP_S,
      gl.CLAMP_TO_EDGE,
    ], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]
  ) gl.texParameteri(gl.TEXTURE_2D, k, v);

  // Until the bridge says otherwise, assume the driver's default 90-degree eyes.
  const shared: XrShared = {
    sequence: 0,
    sentAt: new Map(),
    viewsSent: false,
    hasPicture: false,
    fov: { left: 1, right: 1, up: 1, down: 1 },
    bridgeReady: false,
  };
  let socket: WebSocket | undefined;
  if (options.bridge) {
    const url = new URL(options.bridge, location.href);
    if (url.protocol !== "ws:" && url.protocol !== "wss:") {
      throw Error("XR bridge URL must use WebSocket");
    }
    socket = new WebSocket(url);
    socket.onmessage = (event) => {
      const hello = JSON.parse(event.data);
      if (hello?.version !== 1 || hello.type !== "xr-bridge") return socket!.close();
      const t = hello.source?.fovTan as SourceFov | undefined;
      if (
        [t?.left, t?.right, t?.up, t?.down].every((n) =>
          typeof n === "number" && n > 0.05 && n < 10
        )
      ) shared.fov = t!;
      shared.bridgeReady = true;
    };
    socket.onclose = () => {
      shared.bridgeReady = false;
      options.status("XR · bridge disconnected");
    };
  }
  const send = (message: unknown) => {
    // Poses are latest-wins: skip a frame rather than queue latency behind a slow link.
    if (
      socket && shared.bridgeReady && socket.readyState === WebSocket.OPEN &&
      socket.bufferedAmount < 8192
    ) socket.send(JSON.stringify(message));
  };
  const context: XrContext = { gl, session, layer, space, rates, texture, options, shared, send };
  const stats = options.stats;
  const failed = (what: string, error: unknown) => {
    const text = `${what}: ${error instanceof Error ? error.message : error}`;
    stats?.presenterErrors.push(text);
    if (stats && stats.presenterErrors.length > 20) stats.presenterErrors.shift();
    console.error(text);
  };

  let presenter: Presenter;
  try {
    presenter = (await firstModule).createPresenter(context);
  } catch (error) {
    await session.end();
    throw error;
  }
  if (stats) stats.presenterBuild = build;
  let currentBuild = build;
  // The previous presenter stays ready until the new one has drawn this many frames cleanly.
  let fallback: Presenter | undefined, probation = 0;
  function render(time: number, frame: XRFrame) {
    session.requestAnimationFrame(render);
    try {
      presenter.frame(time, frame);
      if (fallback && ++probation >= 30) {
        fallback.dispose();
        fallback = undefined;
      }
    } catch (error) {
      failed("presenter frame", error);
      if (fallback) {
        presenter.dispose();
        presenter = fallback;
        fallback = undefined;
      }
    }
  }
  session.requestAnimationFrame(render);

  let swapping = false;
  const poll = setInterval(async () => {
    if (swapping) return;
    swapping = true;
    try {
      const next = await fetchBuild();
      if (next === currentBuild) return;
      currentBuild = next;
      const candidate = (await importPresenter(next)).createPresenter(context);
      fallback?.dispose();
      fallback = presenter;
      presenter = candidate;
      probation = 0;
      if (stats) stats.presenterBuild = next;
      console.info(`XR presenter swapped to build ${next}`);
    } catch (error) {
      failed("presenter swap", error);
    } finally {
      swapping = false;
    }
  }, 1000);

  let ended = false;
  const heartbeat = setInterval(() => send({ type: "heartbeat" }), 2000);
  session.addEventListener("end", () => {
    ended = true;
    clearInterval(poll);
    clearInterval(heartbeat);
    fallback?.dispose();
    presenter.dispose();
    socket?.close();
    options.onEnd();
  });
  return () => {
    if (!ended) void session.end();
  };
}
