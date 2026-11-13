/**
 * What part of the decoded picture is on screen. `full` is the whole picture of a
 * single-view source; `stereo` is both halves of a side-by-side source.
 */
export type StreamView = "left" | "right" | "stereo" | "full";
/** True when the view shows the complete decoded picture rather than one half. */
export function showsWholePicture(view: StreamView): boolean {
  return view === "stereo" || view === "full";
}
/** Map a displayed coordinate back into the source picture. */
export function streamX(view: StreamView, x: number): number {
  return showsWholePicture(view) ? x : ((view === "right" ? 1 : 0) + x) / 2;
}
export function drawStreamFrame(
  context: CanvasRenderingContext2D,
  frame: VideoFrame,
  canvas: HTMLCanvasElement,
  view: StreamView,
): void {
  const eyeWidth = frame.displayWidth / 2;
  const whole = showsWholePicture(view);
  context.drawImage(
    frame,
    view === "right" ? eyeWidth : 0,
    0,
    whole ? frame.displayWidth : eyeWidth,
    frame.displayHeight,
    0,
    0,
    canvas.width,
    canvas.height,
  );
}
