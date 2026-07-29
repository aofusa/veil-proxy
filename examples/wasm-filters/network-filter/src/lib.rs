//! Sample Proxy-Wasm L4 network filter (F-133)
//!
//! Demonstrates the Proxy-Wasm network filter ABI (`StreamContext`):
//! - Rewrites every `foo` occurrence in downstream (client -> proxy) data to `bar`
//!   (same length, no buffer resize needed).
//! - Rewrites every `bar` occurrence in upstream (backend -> proxy) data to `baz`.
//! - Closes the connection if the downstream data contains the marker `CLOSE_ME`.

use proxy_wasm::traits::*;
use proxy_wasm::types::*;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Info);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> {
        Box::new(NetworkFilterRoot)
    });
}}

struct NetworkFilterRoot;

impl Context for NetworkFilterRoot {}

impl RootContext for NetworkFilterRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::StreamContext)
    }

    fn create_stream_context(&self, context_id: u32) -> Option<Box<dyn StreamContext>> {
        Some(Box::new(NetworkFilter { context_id }))
    }
}

struct NetworkFilter {
    context_id: u32,
}

impl Context for NetworkFilter {}

/// `needle` の全出現を `replacement`（同じ長さ）へ置換する。バッファサイズを
/// 変えないため host 側の set_buffer はレンジ置換のみで済む。
fn replace_same_len(data: &[u8], needle: &[u8], replacement: &[u8]) -> Option<Vec<u8>> {
    debug_assert_eq!(needle.len(), replacement.len());
    if data.len() < needle.len() {
        return None;
    }
    let mut out = data.to_vec();
    let mut changed = false;
    let mut i = 0;
    while i + needle.len() <= out.len() {
        if &out[i..i + needle.len()] == needle {
            out[i..i + needle.len()].copy_from_slice(replacement);
            changed = true;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    if changed {
        Some(out)
    } else {
        None
    }
}

impl StreamContext for NetworkFilter {
    fn on_new_connection(&mut self) -> Action {
        log::info!("[network-filter] new connection, context={}", self.context_id);
        Action::Continue
    }

    fn on_downstream_data(&mut self, data_size: usize, _end_of_stream: bool) -> Action {
        let Some(data) = self.get_downstream_data(0, data_size) else {
            return Action::Continue;
        };

        if data.windows(8).any(|w| w == b"CLOSE_ME") {
            log::info!("[network-filter] CLOSE_ME marker seen, closing downstream");
            self.close_downstream();
            return Action::Continue;
        }

        if let Some(rewritten) = replace_same_len(&data, b"foo", b"bar") {
            self.set_downstream_data(0, data.len(), &rewritten);
        }

        Action::Continue
    }

    fn on_upstream_data(&mut self, data_size: usize, _end_of_stream: bool) -> Action {
        let Some(data) = self.get_upstream_data(0, data_size) else {
            return Action::Continue;
        };

        if let Some(rewritten) = replace_same_len(&data, b"bar", b"baz") {
            self.set_upstream_data(0, data.len(), &rewritten);
        }

        Action::Continue
    }

    fn on_downstream_close(&mut self, _peer_type: PeerType) {
        log::info!("[network-filter] downstream closed, context={}", self.context_id);
    }

    fn on_upstream_close(&mut self, _peer_type: PeerType) {
        log::info!("[network-filter] upstream closed, context={}", self.context_id);
    }
}
