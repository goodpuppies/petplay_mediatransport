//! Browser side of `petplay-media link` (see ../src/link.rs): the same ALPN and frame format,
//! built for `wasm32-unknown-unknown`. iroh in a browser is relay-only (no UDP), so this only
//! dials: it connects to a hub's endpoint id through that hub's relay. The hub then negotiates
//! a direct transport over the link.

use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl,
    address_lookup::MemoryLookup,
    endpoint::{Connection, RecvStream, RelayMode, SendStream, presets},
};
use js_sys::{Object, Promise, Reflect};
use std::rc::Rc;
use tokio::sync::Mutex;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

const ALPN: &[u8] = b"petplay/link/1";
const MAX_FRAME: usize = 16 * 1024 * 1024;
const CLOSE_MARKER: u32 = u32::MAX;

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsError::new(&error.to_string()).into()
}

#[wasm_bindgen]
pub struct LinkEndpoint {
    endpoint: Endpoint,
    lookup: MemoryLookup,
}

#[wasm_bindgen]
impl LinkEndpoint {
    /// Bind an endpoint. `relay` replaces n0's public relays (a self-hosted or dev relay).
    pub fn bind(relay: Option<String>) -> Promise {
        future_to_promise(async move {
            let lookup = MemoryLookup::new();
            let mode = match relay {
                Some(url) => RelayMode::custom([url.parse::<RelayUrl>().map_err(js_error)?]),
                None => RelayMode::Default,
            };
            let endpoint = Endpoint::builder(presets::Minimal)
                .relay_mode(mode)
                .address_lookup(lookup.clone())
                .bind()
                .await
                .map_err(js_error)?;
            Ok(LinkEndpoint { endpoint, lookup }.into())
        })
    }

    pub fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Dial a hub's link endpoint by id, through its relay.
    pub fn connect(&self, id: String, relay: String) -> Promise {
        let endpoint = self.endpoint.clone();
        let lookup = self.lookup.clone();
        future_to_promise(async move {
            let id: EndpointId = id.parse().map_err(js_error)?;
            let addr =
                EndpointAddr::new(id).with_relay_url(relay.parse::<RelayUrl>().map_err(js_error)?);
            lookup.add_endpoint_info(addr.clone());
            let connection = endpoint.connect(addr, ALPN).await.map_err(js_error)?;
            let (send, recv) = connection.open_bi().await.map_err(js_error)?;
            Ok(LinkConnection {
                connection,
                send: Rc::new(Mutex::new(send)),
                recv: Rc::new(Mutex::new(recv)),
            }
            .into())
        })
    }

    pub fn close(&self) -> Promise {
        let endpoint = self.endpoint.clone();
        future_to_promise(async move {
            endpoint.close().await;
            Ok(JsValue::UNDEFINED)
        })
    }
}

#[wasm_bindgen]
pub struct LinkConnection {
    connection: Connection,
    send: Rc<Mutex<SendStream>>,
    recv: Rc<Mutex<RecvStream>>,
}

#[wasm_bindgen]
impl LinkConnection {
    /// Send one frame. Calls are written in call order.
    pub fn send(&self, text: String) -> Promise {
        let send = self.send.clone();
        future_to_promise(async move {
            let mut send = send.lock().await;
            let bytes = text.as_bytes();
            send.write_all(&(bytes.len() as u32).to_be_bytes())
                .await
                .map_err(js_error)?;
            send.write_all(bytes).await.map_err(js_error)?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// The next frame as `{data}`, or `{closed: reason}` once the peer is done.
    pub fn recv(&self) -> Promise {
        let recv = self.recv.clone();
        let connection = self.connection.clone();
        future_to_promise(async move {
            let mut recv = recv.lock().await;
            let result = Object::new();
            let closed = |reason: String| {
                Reflect::set(&result, &"closed".into(), &reason.into())
                    .map(|_| result.clone().into())
            };
            let mut header = [0u8; 4];
            if let Err(error) = recv.read_exact(&mut header).await {
                let reason = connection
                    .close_reason()
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| format!("stream ended: {error}"));
                return closed(reason);
            }
            let length = u32::from_be_bytes(header);
            if length == CLOSE_MARKER {
                let mut size = [0u8; 4];
                let mut reason = String::from("closed");
                if recv.read_exact(&mut size).await.is_ok() {
                    let mut bytes = vec![0u8; (u32::from_be_bytes(size) as usize).min(1024)];
                    if recv.read_exact(&mut bytes).await.is_ok() {
                        reason = String::from_utf8_lossy(&bytes).into_owned();
                    }
                }
                return closed(reason);
            }
            if length as usize > MAX_FRAME {
                connection.close(1u32.into(), b"frame too large");
                return closed("frame too large".into());
            }
            let mut body = vec![0u8; length as usize];
            if let Err(error) = recv.read_exact(&mut body).await {
                return closed(format!("stream ended: {error}"));
            }
            let text = String::from_utf8(body).map_err(js_error)?;
            Reflect::set(&result, &"data".into(), &text.into())?;
            Ok(result.into())
        })
    }

    /// Close with a reason the peer receives in-band.
    pub fn close(&self, reason: String) -> Promise {
        let send = self.send.clone();
        let connection = self.connection.clone();
        future_to_promise(async move {
            let mut send = send.lock().await;
            let bytes = &reason.as_bytes()[..reason.len().min(1024)];
            let _ = send.write_all(&CLOSE_MARKER.to_be_bytes()).await;
            let _ = send.write_all(&(bytes.len() as u32).to_be_bytes()).await;
            let _ = send.write_all(bytes).await;
            let _ = send.finish();
            // The peer reads the reason, then the stream's end; it closes the connection.
            drop(connection);
            Ok(JsValue::UNDEFINED)
        })
    }
}
