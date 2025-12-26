//! Host Functions for Proxy-Wasm v0.2.1
//!
//! Implements all required host functions exposed to WASM modules.

mod buffers;
mod headers;
mod http_call;
mod logging;
mod metrics;
mod properties;
mod shared_data;
mod stream;

use wasmtime::Linker;

use super::context::HostState;

/// Add all Proxy-Wasm host functions to the linker
pub fn add_host_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    logging::add_functions(linker)?;
    headers::add_functions(linker)?;
    buffers::add_functions(linker)?;
    stream::add_functions(linker)?;
    properties::add_functions(linker)?;
    http_call::add_functions(linker)?;
    shared_data::add_functions(linker)?;
    metrics::add_functions(linker)?;

    Ok(())
}
