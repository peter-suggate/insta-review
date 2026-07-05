//! Local HTTP listener that CS2 POSTs game state to. Single dedicated
//! thread, no async runtime — payloads are small and arrive at most every
//! ~100 ms (the .cfg's throttle).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ir_types::MarkerKind;
use tracing::{debug, info, warn};

use crate::derive::Differ;
use crate::model::GsiPayload;

pub struct GsiServer {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    pub port: u16,
}

impl GsiServer {
    /// Start listening on 127.0.0.1:`port`. Each derived marker is handed
    /// to `on_marker` (which should stamp it with the capture clock).
    /// If `token` is set, payloads whose auth token doesn't match are
    /// dropped.
    pub fn start(
        port: u16,
        token: Option<String>,
        on_marker: impl Fn(MarkerKind) + Send + 'static,
    ) -> Result<Self, String> {
        let server = tiny_http::Server::http(("127.0.0.1", port))
            .map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();

        let join = std::thread::Builder::new()
            .name("ir-gsi".into())
            .spawn(move || {
                info!(port, "GSI listener up");
                let mut differ = Differ::new();
                while !stop2.load(Ordering::Relaxed) {
                    let Ok(Some(mut request)) =
                        server.recv_timeout(std::time::Duration::from_millis(200))
                    else {
                        continue;
                    };
                    let mut body = String::new();
                    if std::io::Read::read_to_string(request.as_reader(), &mut body).is_err() {
                        let _ = request.respond(tiny_http::Response::empty(400));
                        continue;
                    }
                    match serde_json::from_str::<GsiPayload>(&body) {
                        Ok(payload) => {
                            let authorized = match (&token, &payload.auth) {
                                (None, _) => true,
                                (Some(want), Some(auth)) => {
                                    auth.token.as_deref() == Some(want.as_str())
                                }
                                (Some(_), None) => false,
                            };
                            if authorized {
                                for kind in differ.push(&payload) {
                                    debug!(?kind, "GSI marker");
                                    on_marker(kind);
                                }
                            } else {
                                warn!("GSI payload with bad/missing auth token dropped");
                            }
                        }
                        Err(e) => warn!("unparseable GSI payload: {e}"),
                    }
                    let _ = request.respond(tiny_http::Response::empty(200));
                }
            })
            .map_err(|e| format!("spawn gsi thread: {e}"))?;

        Ok(Self {
            stop,
            join: Some(join),
            port,
        })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
