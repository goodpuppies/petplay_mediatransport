/**
 * WebXR presentation of a side-by-side stereo stream, plus head/controller uplink.
 *
 * Each eye's half of the decoded picture is drawn on the plane z = -1 of that eye's view space,
 * spanning the frustum the source rendered it with. That is geometrically correct regardless
 * of the headset's own field of view: a narrower source just leaves black at the edges.
 * The source eye is assumed to be where the headset eye is now (no reprojection yet).
 */

/** Left-eye frustum as positive tangents; the right eye mirrors it horizontally. */
export type SourceFov = { left: number; right: number; up: number; down: number };
type Vec3 = [number, number, number];
type Quat = [number, number, number, number];

export type XrOptions = {
  /** Newest decoded frame due for display, if any; the caller transfers ownership. */
  takeFrame(now: number): VideoFrame | undefined;
  /** `ws:`/`wss:` URL of the PetPlay XR bridge; omit to view without driving SteamVR. */
  bridge?: string;
  /** Send angular velocity for SteamVR prediction (`?xrAngular=1`). */
  angularVelocity?: boolean;
  status(text: string): void;
  onEnd(): void;
};

const vertexSource = `#version 300 es
in vec2 corner;
uniform vec4 tangents; // left, right, down, up as signed tangents on z = -1
uniform vec2 uRange;
uniform mat4 clipFromEye;
out vec2 uv;
void main() {
  vec3 p = vec3(mix(tangents.x, tangents.y, corner.x), mix(tangents.z, tangents.w, corner.y), -1.0);
  // Decoded rows are top-down: the top of the picture (corner.y = 1) is texture v = 0.
  uv = vec2(mix(uRange.x, uRange.y, corner.x), 1.0 - corner.y);
  gl_Position = clipFromEye * vec4(p, 1.0);
}`;
const fragmentSource = `#version 300 es
precision mediump float;
uniform sampler2D picture;
in vec2 uv;
out vec4 color;
void main() { color = vec4(texture(picture, uv).rgb, 1.0); }`;

function compile(gl: WebGL2RenderingContext) {
  const shader = (type: number, source: string) => {
    const s = gl.createShader(type)!;
    gl.shaderSource(s, source);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw Error(gl.getShaderInfoLog(s) ?? "");
    return s;
  };
  const program = gl.createProgram()!;
  gl.attachShader(program, shader(gl.VERTEX_SHADER, vertexSource));
  gl.attachShader(program, shader(gl.FRAGMENT_SHADER, fragmentSource));
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw Error(gl.getProgramInfoLog(program) ?? "");
  return program;
}

const point3 = (p: DOMPointReadOnly, limit: number): Vec3 =>
  [p.x, p.y, p.z].map((n) => Math.max(-limit, Math.min(limit, n))) as Vec3;
const quat = (q: DOMPointReadOnly): Quat => [q.x, q.y, q.z, q.w];
const unit = (n: number | undefined) => Math.max(0, Math.min(1, n ?? 0));
function quatMul(a: Quat, b: Quat): Quat {
  const [ax, ay, az, aw] = a, [bx, by, bz, bw] = b;
  return [
    aw * bx + ax * bw + ay * bz - az * by,
    aw * by - ax * bz + ay * bw + az * bx,
    aw * bz + ax * by - ay * bx + az * bw,
    aw * bw - ax * bx - ay * by - az * bz,
  ];
}

export async function xrSupported(): Promise<boolean> {
  try {
    return !!navigator.xr && await navigator.xr.isSessionSupported("immersive-vr");
  } catch {
    return false;
  }
}

export async function startXr(options: XrOptions): Promise<() => void> {
  const xr = navigator.xr;
  if (!xr) throw Error("WebXR is unavailable");
  // Must run inside the click handler's user activation.
  const session = await xr.requestSession("immersive-vr", { requiredFeatures: ["local-floor"] });
  const canvas = document.createElement("canvas");
  const gl = canvas.getContext("webgl2", { xrCompatible: true, alpha: false, antialias: false });
  if (!gl) {
    await session.end();
    throw Error("WebGL2 is unavailable");
  }
  const layer = new XRWebGLLayer(session, gl, { antialias: false, depth: false });
  session.updateRenderState({ baseLayer: layer, depthNear: 0.05, depthFar: 100 });
  const space = await session.requestReferenceSpace("local-floor");

  const program = compile(gl);
  const corners = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, corners);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]), gl.STATIC_DRAW);
  const vao = gl.createVertexArray();
  gl.bindVertexArray(vao);
  const cornerAttribute = gl.getAttribLocation(program, "corner");
  gl.enableVertexAttribArray(cornerAttribute);
  gl.vertexAttribPointer(cornerAttribute, 2, gl.FLOAT, false, 0, 0);
  const uniform = (name: string) => gl.getUniformLocation(program, name);
  const [tangentsAt, rangeAt, clipAt, pictureAt] = ["tangents", "uRange", "clipFromEye", "picture"]
    .map(uniform);
  const texture = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, texture);
  for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
    [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) {
    gl.texParameteri(gl.TEXTURE_2D, k, v);
  }
  let hasPicture = false;

  // Until the bridge says otherwise, assume the driver's default 90-degree eyes.
  let fov: SourceFov = { left: 1, right: 1, up: 1, down: 1 };
  let socket: WebSocket | undefined, bridgeReady = false, sequence = 0, viewsSent = false;
  if (options.bridge) {
    const url = new URL(options.bridge, location.href);
    if (url.protocol !== "ws:" && url.protocol !== "wss:") throw Error("XR bridge URL must use WebSocket");
    socket = new WebSocket(url);
    socket.onmessage = (event) => {
      const hello = JSON.parse(event.data);
      if (hello?.version !== 1 || hello.type !== "xr-bridge") return socket!.close();
      const t = hello.source?.fovTan;
      if ([t?.left, t?.right, t?.up, t?.down].every((n) => typeof n === "number" && n > 0.05 && n < 10)) {
        fov = t;
      }
      bridgeReady = true;
    };
    socket.onclose = () => {
      bridgeReady = false;
      options.status("XR · bridge disconnected");
    };
  }
  const send = (message: unknown) => {
    // Poses are latest-wins: skip a frame rather than queue latency behind a slow link.
    if (socket && bridgeReady && socket.readyState === WebSocket.OPEN && socket.bufferedAmount < 8192) {
      socket.send(JSON.stringify(message));
    }
  };

  let frames = 0, uploads = 0, lastReport = performance.now();
  function uplink(frame: XRFrame, viewer: XRViewerPose) {
    if (!viewsSent && bridgeReady && viewer.views.length === 2) {
      // Eye offsets relative to the head, for a suggested matching driver profile.
      const headInverse = viewer.transform.inverse;
      const headRotation = quat(headInverse.orientation);
      const rotate = (p: DOMPointReadOnly): Vec3 => {
        const q = headRotation;
        const v = quatMul(quatMul(q, [p.x, p.y, p.z, 0]), [-q[0], -q[1], -q[2], q[3]]);
        const t = headInverse.position;
        return [v[0] + t.x, v[1] + t.y, v[2] + t.z];
      };
      send({
        version: 1,
        type: "xr-views",
        views: viewer.views.map((view) => ({
          eye: view.eye,
          projection: Array.from(view.projectionMatrix),
          position: rotate(view.transform.position),
          rotation: quatMul(headRotation, quat(view.transform.orientation)),
        })),
        recommendedWidth: Math.round(layer.framebufferWidth / 2),
        recommendedHeight: layer.framebufferHeight,
      });
      viewsSent = true;
    }
    const head: Record<string, unknown> = {
      position: point3(viewer.transform.position, 100),
      rotation: quat(viewer.transform.orientation),
    };
    if (viewer.linearVelocity) head.velocity = point3(viewer.linearVelocity, 20);
    if (options.angularVelocity && viewer.angularVelocity) {
      head.angularVelocity = point3(viewer.angularVelocity, 40);
    }
    const message: Record<string, unknown> = { version: 1, type: "xr-frame", sequence: ++sequence, head };
    // Quest Browser never exposes the menu button: both sticks pressed together stand in for
    // the left menu button (VRChat's menus), and neither stick reports its own click meanwhile.
    const stickDown = (side: XRHandedness) =>
      // XRInputSourceArray is array-like only: Quest Browser has no .some() on it.
      Array.from(session.inputSources).some((s) => s.handedness === side && !!s.gamepad?.buttons[3]?.pressed);
    const menuChord = stickDown("left") && stickDown("right");
    for (const source of session.inputSources) {
      if (source.handedness === "none" || !source.gripSpace) continue;
      const grip = frame.getPose(source.gripSpace, space);
      const hand: Record<string, unknown> = {};
      if (grip) {
        hand.pose = { position: point3(grip.transform.position, 100), rotation: quat(grip.transform.orientation) };
      }
      // xr-standard mapping: 0 trigger, 1 squeeze, 3 stick press, 4 A/X, 5 B/Y; axes 2/3 stick.
      const pad = source.gamepad;
      if (pad && pad.mapping === "xr-standard") {
        const b = pad.buttons;
        hand.input = {
          trigger: unit(b[0]?.value),
          grip: unit(b[1]?.value),
          joystick: [Math.max(-1, Math.min(1, pad.axes[2] ?? 0)), Math.max(-1, Math.min(1, -(pad.axes[3] ?? 0)))],
          joystickClick: !menuChord && !!b[3]?.pressed,
          a: !!b[4]?.pressed,
          b: !!b[5]?.pressed,
          system: menuChord && source.handedness === "left",
        };
      }
      message[source.handedness] = hand;
    }
    send(message);
  }

  function render(time: number, frame: XRFrame) {
    session.requestAnimationFrame(render);
    const viewer = frame.getViewerPose(space);
    const picture = options.takeFrame(time);
    if (picture) {
      try {
        gl!.bindTexture(gl!.TEXTURE_2D, texture);
        gl!.texImage2D(gl!.TEXTURE_2D, 0, gl!.RGBA, gl!.RGBA, gl!.UNSIGNED_BYTE, picture);
        hasPicture = true;
        uploads++;
      } finally {
        picture.close();
      }
    }
    if (!viewer) return;
    uplink(frame, viewer);
    gl!.bindFramebuffer(gl!.FRAMEBUFFER, layer.framebuffer);
    gl!.disable(gl!.DEPTH_TEST);
    gl!.clearColor(0, 0, 0, 1);
    gl!.clear(gl!.COLOR_BUFFER_BIT);
    if (hasPicture) {
      gl!.useProgram(program);
      gl!.bindVertexArray(vao);
      gl!.activeTexture(gl!.TEXTURE0);
      gl!.bindTexture(gl!.TEXTURE_2D, texture);
      gl!.uniform1i(pictureAt, 0);
      for (const view of viewer.views) {
        const viewport = layer.getViewport(view);
        if (!viewport) continue;
        gl!.viewport(viewport.x, viewport.y, viewport.width, viewport.height);
        const right = view.eye === "right";
        // Mirror the left-eye frustum for the right eye; each eye owns half the picture.
        gl!.uniform4f(tangentsAt, right ? -fov.right : -fov.left, right ? fov.left : fov.right, -fov.down, fov.up);
        gl!.uniform2f(rangeAt, right ? 0.5 : 0, right ? 1 : 0.5);
        gl!.uniformMatrix4fv(clipAt, false, view.projectionMatrix);
        gl!.drawArrays(gl!.TRIANGLE_STRIP, 0, 4);
      }
    }
    frames++;
    if (time - lastReport > 1000) {
      options.status(`XR · ${frames} fps · ${uploads} new pictures/s${
        options.bridge ? bridgeReady ? " · driving SteamVR" : " · bridge connecting" : ""
      }`);
      frames = uploads = 0;
      lastReport = time;
    }
  }
  session.requestAnimationFrame(render);
  let ended = false;
  session.addEventListener("end", () => {
    ended = true;
    socket?.close();
    options.onEnd();
  });
  const heartbeat = setInterval(() => send({ type: "heartbeat" }), 2000);
  session.addEventListener("end", () => clearInterval(heartbeat));
  return () => {
    if (!ended) void session.end();
  };
}
