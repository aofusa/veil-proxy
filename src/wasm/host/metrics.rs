//! Metrics Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;
use crate::wasm::types::{Metric, MetricValue};

/// Helper to read string from WASM memory
fn read_string(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
    let memory = caller.get_export("memory")?;
    let memory = memory.into_memory()?;
    let data = memory.data(caller);

    let start = ptr as usize;
    let end = start + len as usize;

    if end > data.len() {
        return None;
    }

    String::from_utf8(data[start..end].to_vec()).ok()
}

/// Add metrics functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_define_metric
    linker.func_wrap(
        "env",
        "proxy_define_metric",
        |mut caller: Caller<'_, HostState>,
         metric_type: i32,
         name_ptr: i32,
         name_size: i32,
         return_metric_id: i32|
         -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_metrics {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            // Read name
            let name = match read_string(&caller, name_ptr, name_size) {
                Some(n) => n,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Create metric
            let state = caller.data_mut();
            let id = state.http_ctx.allocate_metric_id();

            let value = match metric_type {
                METRIC_TYPE_COUNTER => MetricValue::Counter(0),
                METRIC_TYPE_GAUGE => MetricValue::Gauge(0),
                METRIC_TYPE_HISTOGRAM => MetricValue::Histogram(Vec::new()),
                _ => return PROXY_RESULT_BAD_ARGUMENT,
            };

            state.http_ctx.metrics.insert(
                id,
                Metric {
                    name,
                    metric_type,
                    value,
                },
            );

            // Write metric ID
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data_mut(&mut caller);
            let ptr = return_metric_id as usize;

            if ptr + 4 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            data[ptr..ptr + 4].copy_from_slice(&id.to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    // proxy_increment_metric
    linker.func_wrap(
        "env",
        "proxy_increment_metric",
        |mut caller: Caller<'_, HostState>, metric_id: i32, offset: i64| -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_metrics {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            let state = caller.data_mut();
            match state.http_ctx.metrics.get_mut(&metric_id) {
                Some(metric) => match &mut metric.value {
                    MetricValue::Counter(v) => *v += offset,
                    MetricValue::Gauge(v) => *v += offset,
                    _ => return PROXY_RESULT_BAD_ARGUMENT,
                },
                None => return PROXY_RESULT_NOT_FOUND,
            }

            PROXY_RESULT_OK
        },
    )?;

    // proxy_record_metric
    linker.func_wrap(
        "env",
        "proxy_record_metric",
        |mut caller: Caller<'_, HostState>, metric_id: i32, value: i64| -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_metrics {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            let state = caller.data_mut();
            match state.http_ctx.metrics.get_mut(&metric_id) {
                Some(metric) => match &mut metric.value {
                    MetricValue::Counter(v) => *v = value,
                    MetricValue::Gauge(v) => *v = value,
                    MetricValue::Histogram(h) => h.push(value as u64),
                },
                None => return PROXY_RESULT_NOT_FOUND,
            }

            PROXY_RESULT_OK
        },
    )?;

    // proxy_get_metric
    linker.func_wrap(
        "env",
        "proxy_get_metric",
        |mut caller: Caller<'_, HostState>, metric_id: i32, return_value: i32| -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_metrics {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            let state = caller.data();
            let value = match state.http_ctx.metrics.get(&metric_id) {
                Some(metric) => match &metric.value {
                    MetricValue::Counter(v) => *v,
                    MetricValue::Gauge(v) => *v,
                    MetricValue::Histogram(h) => h.len() as i64,
                },
                None => return PROXY_RESULT_NOT_FOUND,
            };

            // Write value
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data_mut(&mut caller);
            let ptr = return_value as usize;

            if ptr + 8 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            data[ptr..ptr + 8].copy_from_slice(&value.to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}
