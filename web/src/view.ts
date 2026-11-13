export type StreamView = "left" | "right" | "stereo";
/** Map a displayed single-eye coordinate back into the source side-by-side frame. */
export function streamX(view: StreamView, x: number): number {
  return view === "stereo" ? x : ((view === "right" ? 1 : 0) + x) / 2;
}
export function drawStreamFrame(
  context: CanvasRenderingContext2D,
  frame: VideoFrame,
  canvas: HTMLCanvasElement,
  view: StreamView,
): void {
  const eyeWidth = frame.displayWidth / 2;
  context.drawImage(
    frame,
    view === "right" ? eyeWidth : 0,
    0,
    view === "stereo" ? frame.displayWidth : eyeWidth,
    frame.displayHeight,
    0,
    0,
    canvas.width,
    canvas.height,
  );
}
