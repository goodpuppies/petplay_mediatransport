/**
 * Hot-swappable XR presenter: picture upload, timewarped drawing and head/controller uplink.
 * Built separately to dist/presenter.js; the XR shell (xr.ts) imports a new build while the
 * session keeps running. Everything that must survive a swap lives in the shell's context.
 *
 * Each eye's half of the decoded picture is drawn on the plane z = -1 of that eye's view space,
 * spanning the frustum the source rendered it with, which stays geometrically correct whatever
 * the headset's own field of view. When the stream says which head pose a picture was rendered
 * for, the quad is rotated by the head's rotation since then (rotational timewarp). Head
 * translation is not corrected.
 */
import type { Presenter, Quat, Vec3, XrContext } from "./xrTypes";

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
  const shaders: WebGLShader[] = [];
  const shader = (type: number, source: string) => {
    const s = gl.createShader(type)!;
    shaders.push(s);
    gl.shaderSource(s, source);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw Error(gl.getShaderInfoLog(s) ?? "");
    return s;
  };
  const program = gl.createProgram()!;
  gl.attachShader(program, shader(gl.VERTEX_SHADER, vertexSource));
  gl.attachShader(program, shader(gl.FRAGMENT_SHADER, fragmentSource));
  gl.linkProgram(program);
  for (const s of shaders) gl.deleteShader(s);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    throw Error(gl.getProgramInfoLog(program) ?? "");
  }
  return program;
}

const point3 = (p: DOMPointReadOnly, limit: number): Vec3 =>
  [p.x, p.y, p.z].map((n) => Math.max(-limit, Math.min(limit, n))) as Vec3;
const quat = (q: DOMPointReadOnly): Quat => [q.x, q.y, q.z, q.w];
const unit = (n: number | undefined) => Math.max(0, Math.min(1, n ?? 0));
/** Column-major `projection × rotation(q)`. */
function projectRotation(projection: Float32Array, q: Quat): Float32Array {
  const [x, y, z, w] = q;
  const m = [
    1 - 2 * (y * y + z * z), 2 * (x * y + z * w), 2 * (x * z - y * w), 0,
    2 * (x * y - z * w), 1 - 2 * (x * x + z * z), 2 * (y * z + x * w), 0,
    2 * (x * z + y * w), 2 * (y * z - x * w), 1 - 2 * (x * x + y * y), 0,
    0, 0, 0, 1,
  ];
  const out = new Float32Array(16);
  for (let c = 0; c < 4; c++) {
    for (let r = 0; r < 4; r++) {
      let sum = 0;
      for (let k = 0; k < 4; k++) sum += projection[k * 4 + r] * m[c * 4 + k];
      out[c * 4 + r] = sum;
    }
  }
  return out;
}
const sample = (values: number[], value: number) => {
  values.push(value);
  if (values.length > 600) values.shift();
};
function quatMul(a: Quat, b: Quat): Quat {
  const [ax, ay, az, aw] = a, [bx, by, bz, bw] = b;
  return [
    aw * bx + ax * bw + ay * bz - az * by,
    aw * by - ax * bz + ay * bw + az * bx,
    aw * bz + ax * by - ay * bx + az * bw,
    aw * bw - ax * bx - ay * by - az * bz,
  ];
}

/** Rotation by `omega` (rad/s, reference space) over `seconds`, as a left-multiplied quaternion. */
function spin(omega: Vec3, seconds: number): Quat {
  const rate = Math.hypot(...omega);
  if (rate < 1e-6) return [0, 0, 0, 1];
  const half = rate * seconds / 2, s = Math.sin(half) / rate;
  return [omega[0] * s, omega[1] * s, omega[2] * s, Math.cos(half)];
}
const normalize = (q: Quat): Quat => {
  const n = Math.hypot(...q);
  return q.map((v) => v / n) as Quat;
};

/**
 * Head velocities for prediction: the runtime's own when it reports them, otherwise smoothed
 * differences between consecutive XR frames (which are already predicted to their display time).
 */
function velocityTracker() {
  let last: { time: number; position: Vec3; rotation: Quat } | undefined;
  let linear: Vec3 = [0, 0, 0], angular: Vec3 = [0, 0, 0];
  return (time: number, viewer: XRPose) => {
    const position = [viewer.transform.position.x, viewer.transform.position.y, viewer.transform.position.z] as Vec3;
    const rotation = quat(viewer.transform.orientation);
    const dt = last ? (time - last.time) / 1000 : 0;
    if (last && dt > 0.002 && dt < 0.1) {
      const blend = (a: Vec3, b: Vec3): Vec3 => a.map((v, i) => v + 0.5 * (b[i] - v)) as Vec3;
      linear = blend(linear, position.map((v, i) => (v - last!.position[i]) / dt) as Vec3);
      // World-frame rotation since the last frame: q_now · q_last⁻¹ as axis-angle per second.
      const [lx, ly, lz, lw] = last.rotation;
      let d = quatMul(rotation, [-lx, -ly, -lz, lw]);
      if (d[3] < 0) d = d.map((v) => -v) as Quat;
      const angle = 2 * Math.acos(Math.min(1, d[3])), sine = Math.sqrt(Math.max(0, 1 - d[3] * d[3]));
      const axis: Vec3 = sine > 1e-6 ? [d[0] / sine, d[1] / sine, d[2] / sine] : [0, 0, 0];
      angular = blend(angular, axis.map((v) => v * angle / dt) as Vec3);
    } else if (!last || dt >= 0.1) {
      linear = [0, 0, 0];
      angular = [0, 0, 0];
    }
    last = { time, position, rotation };
    const runtime = viewer.linearVelocity && viewer.angularVelocity;
    return {
      source: runtime ? "runtime" : "frame differences",
      linear: viewer.linearVelocity ? point3(viewer.linearVelocity, 20) : linear,
      angular: viewer.angularVelocity ? point3(viewer.angularVelocity, 40) : angular,
    };
  };
}

export function createPresenter(context: XrContext): Presenter {
  const { gl, session, layer, space, rates, texture, options, shared, send } = context;
  const stats = options.stats;
  const warp = options.warp ?? true;
  const program = compile(gl);
  const corners = gl.createBuffer();
  const vao = gl.createVertexArray();
  gl.bindVertexArray(vao);
  gl.bindBuffer(gl.ARRAY_BUFFER, corners);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]), gl.STATIC_DRAW);
  const cornerAttribute = gl.getAttribLocation(program, "corner");
  gl.enableVertexAttribArray(cornerAttribute);
  gl.vertexAttribPointer(cornerAttribute, 2, gl.FLOAT, false, 0, 0);
  gl.bindVertexArray(null);
  const uniform = (name: string) => gl.getUniformLocation(program, name);
  const [tangentsAt, rangeAt, clipAt, pictureAt] = ["tangents", "uRange", "clipFromEye", "picture"]
    .map(uniform);
  let frames = 0, uploads = 0, lastReport = performance.now();
  const velocities = velocityTracker();
  const handVelocities = { left: velocityTracker(), right: velocityTracker() };
  /** `pose` moved forward by `seconds` along its velocities, as sent to the driver. */
  const predict = (pose: XRPose, velocity: ReturnType<ReturnType<typeof velocityTracker>>, seconds: number) => {
    const p = pose.transform.position;
    return {
      position: [p.x, p.y, p.z].map((v, i) =>
        Math.max(-100, Math.min(100, v + velocity.linear[i] * seconds))
      ) as Vec3,
      rotation: normalize(quatMul(spin(velocity.angular, seconds), quat(pose.transform.orientation))),
    };
  };

  function uplink(frame: XRFrame, viewer: XRViewerPose, time: number) {
    if (!shared.viewsSent && shared.bridgeReady && viewer.views.length === 2) {
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
        supportedFrameRates: rates,
        recommendedWidth: Math.round(layer.framebufferWidth / 2),
        recommendedHeight: layer.framebufferHeight,
      });
      shared.viewsSent = true;
    }
    shared.sentAt.set(shared.sequence + 1, performance.now());
    if (shared.sentAt.size > 1024) shared.sentAt.delete(shared.sentAt.keys().next().value!);
    // Predict the head to when a frame rendered from this pose will be on screen: the measured
    // pose-to-display latency. Timewarp then only corrects the prediction's error.
    const velocity = velocities(time, viewer);
    const recent = stats?.latencyMs.slice(-60).sort((a, b) => a - b) ?? [];
    const scale = Math.max(0, Math.min(1.5, stats?.predictScale ?? 1));
    const ahead = recent.length ? Math.min(150, recent[recent.length >> 1]) * scale / 1000 : 0;
    if (stats) {
      stats.predictMs = ahead * 1000;
      stats.velocitySource = velocity.source;
    }
    const head: Record<string, unknown> = predict(viewer, velocity, ahead);
    head.velocity = velocity.linear.map((v) => Math.max(-20, Math.min(20, v)));
    if (options.angularVelocity && viewer.angularVelocity) {
      head.angularVelocity = point3(viewer.angularVelocity, 40);
    }
    const message: Record<string, unknown> = {
      version: 1,
      type: "xr-frame",
      sequence: ++shared.sequence,
      // Lets the driver map this frame onto its own clock and latch the hands at a steady lag.
      time,
      head,
    };
    // XRInputSourceArray is array-like only: Quest Browser has no .some() on it.
    const sources = Array.from(session.inputSources);
    // Quest Browser never exposes the menu button: both sticks pressed together stand in for
    // the left menu button (VRChat's menus), and neither stick reports its own click meanwhile.
    const stickDown = (side: XRHandedness) =>
      sources.some((s) => s.handedness === side && !!s.gamepad?.buttons[3]?.pressed);
    const menuChord = stickDown("left") && stickDown("right");
    for (const source of sources) {
      if (source.handedness === "none" || !source.gripSpace) continue;
      const grip = frame.getPose(source.gripSpace, space);
      const hand: Record<string, unknown> = {};
      if (grip) {
        // Controllers are predicted by the same horizon as the head, so hands and head in a
        // frame match where they will be when it is shown.
        const handVelocity = handVelocities[source.handedness](time, grip);
        if (traceRow && source.handedness === "right") {
          const g = grip.transform.position;
          traceRow.raw = [g.x, g.y, g.z].map((v) => +v.toFixed(5));
          traceRow.vs = handVelocity.source;
          traceRow.vel = handVelocity.linear.map((v) => +v.toFixed(4));
          traceRow.ahead = +(ahead * 1000).toFixed(1);
          traceRow.sent = shared.sequence;
        }
        hand.pose = {
          ...predict(grip, handVelocity, ahead),
          // The driver carries the pose along these to each display frame's moment.
          velocity: handVelocity.linear.map((v) => Math.max(-20, Math.min(20, v))),
          angularVelocity: handVelocity.angular.map((v) => Math.max(-40, Math.min(40, v))),
        };
      }
      // xr-standard mapping: 0 trigger, 1 squeeze, 3 stick press, 4 A/X, 5 B/Y; axes 2/3 stick.
      const pad = source.gamepad;
      if (pad && pad.mapping === "xr-standard") {
        const b = pad.buttons;
        hand.input = {
          trigger: unit(b[0]?.value),
          grip: unit(b[1]?.value),
          joystick: [
            Math.max(-1, Math.min(1, pad.axes[2] ?? 0)),
            Math.max(-1, Math.min(1, -(pad.axes[3] ?? 0))),
          ],
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

  /** Opt-in per-frame trace for pacing analysis: set `petplayStream.handTrace = []`. */
  const trace = () => (stats as { handTrace?: unknown[] } | undefined)?.handTrace;
  let traceRow: Record<string, unknown> | undefined;
  function frame(time: number, xrFrame: XRFrame) {
    const viewer = xrFrame.getViewerPose(space);
    const picture = options.takeFrame(time);
    const rows = trace();
    const callbackStart = performance.now();
    traceRow = rows
      ? { t: +time.toFixed(2), pic: picture ? picture.timestamp : null, start: +callbackStart.toFixed(2) }
      : undefined;
    if (traceRow && picture) {
      const times = (globalThis as { petplayPictureTimes?: Record<string, Map<number, number>> })
        .petplayPictureTimes;
      traceRow.arr = times?.arrivedAt.get(picture.timestamp);
      traceRow.dec = times?.decodedAt.get(picture.timestamp);
      traceRow.now = +performance.now().toFixed(2);
    }
    if (rows && traceRow) {
      rows.push(traceRow);
      if (rows.length > 4000) rows.shift();
    }
    if (picture) {
      try {
        gl.bindTexture(gl.TEXTURE_2D, texture);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, picture);
        shared.hasPicture = true;
        uploads++;
        const pose = options.poseFor?.(picture.timestamp);
        shared.shownPose = pose;
        if (traceRow) traceRow.seq = pose?.sequence;
        if (pose) {
          if (stats) stats.posed++;
          const sent = shared.sentAt.get(pose.sequence);
          if (sent !== undefined && stats) sample(stats.latencyMs, time - sent);
        } else if (stats) stats.unposed++;
      } finally {
        picture.close();
      }
    }
    if (!viewer) return;
    uplink(xrFrame, viewer, time);
    gl.bindFramebuffer(gl.FRAMEBUFFER, layer.framebuffer);
    gl.disable(gl.DEPTH_TEST);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (shared.hasPicture) {
      const fov = shared.fov;
      gl.useProgram(program);
      gl.bindVertexArray(vao);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, texture);
      gl.uniform1i(pictureAt, 0);
      for (const view of viewer.views) {
        const viewport = layer.getViewport(view);
        if (!viewport) continue;
        gl.viewport(viewport.x, viewport.y, viewport.width, viewport.height);
        const right = view.eye === "right";
        // Mirror the left-eye frustum for the right eye; each eye owns half the picture.
        gl.uniform4f(
          tangentsAt,
          right ? -fov.right : -fov.left,
          right ? fov.left : fov.right,
          -fov.down,
          fov.up,
        );
        gl.uniform2f(rangeAt, right ? 0.5 : 0, right ? 1 : 0.5);
        // Rotation from the head the picture was rendered for to this eye now:
        // v_now = q_now⁻¹ q_src v.
        let clip = view.projectionMatrix;
        if (warp && shared.shownPose) {
          const o = view.transform.orientation;
          const delta = quatMul([-o.x, -o.y, -o.z, o.w], shared.shownPose.rotation);
          clip = projectRotation(view.projectionMatrix, delta);
          if (stats && view.eye === "left") {
            sample(
              stats.warpDegrees,
              2 * Math.acos(Math.min(1, Math.abs(delta[3]))) * 180 / Math.PI,
            );
          }
        }
        gl.uniformMatrix4fv(clipAt, false, clip);
        gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      }
      gl.bindVertexArray(null);
    }
    if (traceRow) {
      // Main-thread time spent in this XR callback (upload, uplink, draw submission).
      traceRow.cb = +(performance.now() - (traceRow.start as number)).toFixed(2);
    }
    frames++;
    if (time - lastReport > 1000) {
      const l = stats?.latencyMs.slice(-60).sort((a, b) => a - b);
      if (stats) stats.displayRate = session.frameRate;
      options.status(`XR · ${frames} fps · ${uploads} new pictures/s${
        l?.length ? ` · pose→display ${l[l.length >> 1].toFixed(0)} ms` : ""
      }${warp ? "" : " · warp off"}${
        options.bridge ? shared.bridgeReady ? " · driving SteamVR" : " · bridge connecting" : ""
      }`);
      frames = uploads = 0;
      lastReport = time;
    }
  }

  return {
    frame,
    dispose() {
      gl.deleteVertexArray(vao);
      gl.deleteBuffer(corners);
      gl.deleteProgram(program);
    },
  };
}
