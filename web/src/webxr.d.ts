// The minimal WebXR Device API surface the viewer uses. TypeScript's DOM library omits WebXR.
interface XRSystem {
  isSessionSupported(mode: "immersive-vr" | "immersive-ar"): Promise<boolean>;
  requestSession(
    mode: "immersive-vr" | "immersive-ar",
    init?: { requiredFeatures?: string[]; optionalFeatures?: string[] },
  ): Promise<XRSession>;
  /** Lets the browser show its own entry prompt; resolves with the session it starts. */
  offerSession?(
    mode: "immersive-vr" | "immersive-ar",
    init?: { requiredFeatures?: string[]; optionalFeatures?: string[] },
  ): Promise<XRSession>;
}
interface Navigator {
  readonly xr?: XRSystem;
}
interface WebGLContextAttributes {
  xrCompatible?: boolean;
}
type XREye = "left" | "right" | "none";
type XRHandedness = "left" | "right" | "none";
interface XRRigidTransform {
  readonly position: DOMPointReadOnly;
  readonly orientation: DOMPointReadOnly;
  readonly matrix: Float32Array;
  readonly inverse: XRRigidTransform;
}
interface XRPose {
  readonly transform: XRRigidTransform;
  readonly linearVelocity?: DOMPointReadOnly;
  readonly angularVelocity?: DOMPointReadOnly;
  readonly emulatedPosition: boolean;
}
interface XRView {
  readonly eye: XREye;
  readonly projectionMatrix: Float32Array;
  readonly transform: XRRigidTransform;
}
interface XRViewerPose extends XRPose {
  readonly views: ReadonlyArray<XRView>;
}
interface XRSpace extends EventTarget {}
interface XRReferenceSpace extends XRSpace {}
interface XRInputSource {
  readonly handedness: XRHandedness;
  readonly gripSpace?: XRSpace;
  readonly gamepad?: Gamepad;
}
interface XRInputSourceArray extends Iterable<XRInputSource> {
  readonly length: number;
  readonly [index: number]: XRInputSource;
}
interface XRFrame {
  readonly session: XRSession;
  getViewerPose(space: XRReferenceSpace): XRViewerPose | undefined;
  getPose(space: XRSpace, base: XRSpace): XRPose | undefined;
}
interface XRViewport {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
}
declare class XRWebGLLayer {
  constructor(
    session: XRSession,
    context: WebGL2RenderingContext,
    init?: { alpha?: boolean; antialias?: boolean; depth?: boolean; framebufferScaleFactor?: number },
  );
  readonly framebuffer: WebGLFramebuffer | null;
  readonly framebufferWidth: number;
  readonly framebufferHeight: number;
  getViewport(view: XRView): XRViewport | undefined;
}
interface XRSession extends EventTarget {
  /** Array-like and iterable, not an Array: no .some()/.map() in real browsers. */
  readonly inputSources: XRInputSourceArray;
  readonly renderState: { readonly baseLayer?: XRWebGLLayer };
  updateRenderState(state: { baseLayer?: XRWebGLLayer; depthNear?: number; depthFar?: number }): void;
  requestReferenceSpace(type: "local-floor" | "local"): Promise<XRReferenceSpace>;
  requestAnimationFrame(callback: (time: number, frame: XRFrame) => void): number;
  readonly supportedFrameRates?: Float32Array;
  readonly frameRate?: number;
  updateTargetFrameRate?(rate: number): Promise<void>;
  end(): Promise<void>;
}
