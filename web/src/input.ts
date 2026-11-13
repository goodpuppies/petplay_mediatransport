import { type StreamView, streamX } from "./view";
/** Thin input sender. Scene, camera rays, capture semantics, and gizmos stay in PetPlay. */
export function attachRemoteInput(
  canvas: HTMLCanvasElement,
  address: string,
  getView: () => StreamView = () => "stereo",
): () => void {
  const url = new URL(address, location.href);
  if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw Error("Input URL must use WebSocket");
  }
  const socket = new WebSocket(url);
  let sequence = 0, viewport = "", ready = false;
  const status = document.createElement("span");
  status.textContent = " · Input connecting";
  document.querySelector("header")?.append(status);
  const send = (message: unknown) => {
    if (ready && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(message));
    }
  };
  const message = (event: MessageEvent) => {
    const config = JSON.parse(event.data);
    if (config.version !== 1 || config.viewport !== "steamvr-stereo") {
      socket.close();
      return;
    }
    viewport = config.viewport;
    ready = true;
    status.textContent = " · Input connected";
  };
  socket.addEventListener("message", message);
  socket.addEventListener("close", () => {
    ready = false;
    status.textContent = " · Input disconnected";
  });
  socket.addEventListener("error", () => {
    status.textContent = " · Input connection failed";
  });
  canvas.style.touchAction = "none";
  const pointer = (event: PointerEvent | WheelEvent) => {
    if (!ready) return;
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    send({
      version: 1,
      viewport,
      sequence: ++sequence,
      type: event.type,
      pointerId: "pointerId" in event ? event.pointerId : 1,
      pointerType: "pointerType" in event ? event.pointerType : "mouse",
      x: streamX(getView(), (event.clientX - rect.left) / rect.width),
      y: (event.clientY - rect.top) / rect.height,
      button: event.button,
      buttons: event.buttons,
      deltaY: "deltaY" in event
        ? event.deltaY *
          (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? rect.height : 1)
        : 0,
      ctrlKey: event.ctrlKey,
      shiftKey: event.shiftKey,
      altKey: event.altKey,
      metaKey: event.metaKey,
    });
    if (event.type === "pointerdown" && "pointerId" in event) {
      canvas.setPointerCapture(event.pointerId);
    }
    event.preventDefault();
  };
  const cancel = () => send({ type: "cancel" });
  const contextMenu = (event: Event) => event.preventDefault();
  const events = [
    "pointermove",
    "pointerdown",
    "pointerup",
    "pointercancel",
    "pointerleave",
    "wheel",
  ] as const;
  for (const type of events) {
    canvas.addEventListener(type, pointer as EventListener, { passive: false });
  }
  canvas.addEventListener("contextmenu", contextMenu);
  canvas.addEventListener("lostpointercapture", cancel);
  addEventListener("blur", cancel);
  const visibility = () => {
    if (document.hidden) cancel();
  };
  document.addEventListener("visibilitychange", visibility);
  const heartbeat = setInterval(() => send({ type: "heartbeat" }), 2000);
  const close = () => {
    cancel();
    clearInterval(heartbeat);
    socket.close();
    for (const type of events) {
      canvas.removeEventListener(type, pointer as EventListener);
    }
    canvas.removeEventListener("contextmenu", contextMenu);
    canvas.removeEventListener("lostpointercapture", cancel);
    removeEventListener("blur", cancel);
    document.removeEventListener("visibilitychange", visibility);
    removeEventListener("pagehide", close);
    status.remove();
  };
  addEventListener("pagehide", close, { once: true });
  return close;
}
