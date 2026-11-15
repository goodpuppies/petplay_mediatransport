/**
 * The contract between the XR shell (xr.ts, in viewer.js) and the hot-swappable presenter
 * (presenter.ts, built to presenter.js). The shell owns everything that must survive a swap:
 * the session, GL context, layer, bridge socket, picture texture and the pose sequence.
 */
export type Vec3 = [number, number, number];
export type Quat = [number, number, number, number];
/** Left-eye frustum as positive tangents; the right eye mirrors it horizontally. */
export type SourceFov = { left: number; right: number; up: number; down: number };
/** The head pose a picture was rendered for, from the source's `pose` track. */
export type SourcePose = { sequence: number; rotation: Quat; position: Vec3; ageUs: number };
export type XrStats = {
  latencyMs: number[];
  warpDegrees: number[];
  posed: number;
  unposed: number;
  displayRate?: number;
  presenterBuild?: string;
  presenterErrors: string[];
  /** Fraction of the measured latency the sent head pose is predicted ahead (live-tunable). */
  predictScale?: number;
  /** Prediction actually applied, and where the velocities came from. */
  predictMs?: number;
  velocitySource?: string;
};
export type XrOptions = {
  /** Picture due for display, if any; the caller transfers ownership. */
  takeFrame(now: number): VideoFrame | undefined;
  /** Head pose the picture with this timestamp was rendered for, if the source sends poses. */
  poseFor?(timestamp: number): SourcePose | undefined;
  /** Source frame rate; the headset is asked for it when it supports that rate. */
  framerate?: number;
  /** Rotational timewarp (default on; `?warp=0` turns it off for comparison). */
  warp?: boolean;
  stats?: XrStats;
  /** `ws:`/`wss:` URL of the PetPlay XR bridge; omit to view without driving SteamVR. */
  bridge?: string;
  /** Send angular velocity for SteamVR prediction (`?xrAngular=1`). */
  angularVelocity?: boolean;
  status(text: string): void;
  onEnd(): void;
};
/** Mutable state that outlives presenters. The bridge rejects a sequence that goes backwards. */
export type XrShared = {
  sequence: number;
  sentAt: Map<number, number>;
  viewsSent: boolean;
  hasPicture: boolean;
  shownPose?: SourcePose;
  fov: SourceFov;
  bridgeReady: boolean;
};
export type XrContext = {
  gl: WebGL2RenderingContext;
  session: XRSession;
  layer: XRWebGLLayer;
  space: XRReferenceSpace;
  rates: number[];
  texture: WebGLTexture;
  options: XrOptions;
  shared: XrShared;
  send(message: unknown): void;
};
export type Presenter = { frame(time: number, frame: XRFrame): void; dispose(): void };
export type PresenterModule = { createPresenter(context: XrContext): Presenter };
