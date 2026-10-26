//! Properties Host Functions

use std::sync::Arc;

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// `get_property` の値。
///
/// B-178: 要求のパス・`plugin_data` 等は共有している `Arc` をそのまま持ち、ゲストのメモリへ 1 回写すだけにする
/// （以前は呼ぶたびに `Vec` へ複製していた。`data_files` は数 MB もあり得る）。
pub(crate) enum PropertyValue {
    /// 共有しているバイト列（`plugin_data.<name>`）
    Shared(Arc<[u8]>),
    /// 共有している文字列（パス・メソッド・クライアントの IP・プラグイン名）
    Str(Arc<str>),
    /// 定数
    Static(&'static [u8]),
    /// その場で作った値
    Owned(Vec<u8>),
}

impl PropertyValue {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        match self {
            PropertyValue::Shared(b) => b,
            PropertyValue::Str(s) => s.as_bytes(),
            PropertyValue::Static(b) => b,
            PropertyValue::Owned(v) => v,
        }
    }
}

/// Get property value by path
pub(crate) fn get_property_value(state: &HostState, path: &str) -> Option<PropertyValue> {
    use PropertyValue::{Owned, Shared, Static, Str};
    // Check capability
    if !state.http_ctx.capabilities.is_property_allowed(path) {
        return None;
    }

    match path {
        // Request properties
        "request.path" | "request.url_path" => Some(Str(state.http_ctx.request_path.clone())),
        "request.host" => state
            .http_ctx
            .request_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(b"host"))
            .map(|(_, v)| Owned(v.clone())),
        "request.method" => Some(Str(state.http_ctx.request_method.clone())),
        "request.scheme" => Some(Static(b"https")),
        "request.protocol" => Some(Static(b"HTTP/1.1")),
        "request.query" => Some(Owned(state.http_ctx.request_query.as_bytes().to_vec())),
        "request.time" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_default();
            Some(Owned(now.into_bytes()))
        }
        "request.total_size" => {
            let size = state.http_ctx.request_body.len();
            Some(Owned(size.to_string().into_bytes()))
        }

        // Response properties
        "response.code" => Some(Owned(
            state.http_ctx.response_status.to_string().into_bytes(),
        )),
        "response.code_details" => Some(Static(b"via veil-proxy")),

        // Connection properties
        "source.address" => Some(Str(state.http_ctx.client_ip.clone())),
        "destination.address" => Some(Static(b"0.0.0.0:0")),

        // Plugin properties
        "plugin_name" => Some(Str(state.http_ctx.plugin_name.clone())),
        "plugin_root_id" => match &state.http_ctx.plugin_root_id {
            Some(r) => Some(Str(r.clone())),
            None => Some(Owned(
                state.http_ctx.root_context_id.to_string().into_bytes(),
            )),
        },

        // 起動時に読んだファイル（`[[wasm.modules]] data_files`。F-271）。複製しない（B-178）
        p if p.starts_with("plugin_data.") => state
            .http_ctx
            .plugin_data
            .get(&p["plugin_data.".len()..])
            .map(|d| Shared(d.clone())),

        // Check custom_properties for user-defined properties
        _ => state
            .http_ctx
            .custom_properties
            .get(path)
            .map(|v| Owned(v.clone())),
    }
}

/// Helper to allocate memory in WASM
///
/// B-20: async store のため `call_async` を使用（同期 `call` は panic する）。
async fn allocate_wasm_memory(caller: &mut Caller<'_, HostState>, size: usize) -> Option<i32> {
    let func = caller
        .get_export("proxy_on_memory_allocate")
        .or_else(|| caller.get_export("malloc"))?;
    let func = func.into_func()?;
    let typed = func.typed::<i32, i32>(&mut *caller).ok()?;
    typed.call_async(&mut *caller, size as i32).await.ok()
}

/// Helper to write to WASM memory
fn write_to_wasm(caller: &mut Caller<'_, HostState>, ptr: i32, data: &[u8]) -> bool {
    let memory = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(mem)) => mem,
        _ => return false,
    };

    let mem_data = memory.data_mut(caller);
    let start = ptr as usize;
    let end = start + data.len();

    if end > mem_data.len() {
        return false;
    }

    mem_data[start..end].copy_from_slice(data);
    true
}

/// Helper to read string from WASM memory
fn read_bytes(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?;
    let memory = memory.into_memory()?;
    let data = memory.data(caller);

    let start = ptr as usize;
    let end = start + len as usize;

    if end > data.len() {
        return None;
    }

    Some(data[start..end].to_vec())
}

/// `.` で繋いだプロパティのパス。短いものはスタックに置く（B-178: 呼ぶたびに確保しない）
pub(crate) struct PropertyPath {
    buf: [u8; 128],
    len: usize,
    heap: Option<String>,
}

impl PropertyPath {
    pub(crate) fn new() -> Self {
        Self {
            buf: [0; 128],
            len: 0,
            heap: None,
        }
    }

    fn push(&mut self, part: &[u8]) {
        if let Some(h) = &mut self.heap {
            h.push_str(&String::from_utf8_lossy(part));
            return;
        }
        if std::str::from_utf8(part).is_ok() && self.len + part.len() <= self.buf.len() {
            self.buf[self.len..self.len + part.len()].copy_from_slice(part);
            self.len += part.len();
        } else {
            let mut h = String::from(self.as_str());
            h.push_str(&String::from_utf8_lossy(part));
            self.heap = Some(h);
        }
    }

    fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }

    pub(crate) fn as_str(&self) -> &str {
        match &self.heap {
            Some(h) => h,
            // push は UTF-8 の要素だけを buf に入れる
            None => std::str::from_utf8(&self.buf[..self.len]).unwrap_or(""),
        }
    }
}

/// [`parse_path`] の確保しない版。読めたら true
pub(crate) fn parse_path_into(data: &[u8], out: &mut PropertyPath) -> bool {
    if parse_len_prefixed_into(data, out) {
        return true;
    }
    *out = PropertyPath::new();
    for part in data.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push(b".");
        }
        out.push(part);
    }
    !out.is_empty()
}

fn parse_len_prefixed_into(data: &[u8], out: &mut PropertyPath) -> bool {
    if data.len() < 4 {
        return false;
    }
    let num_parts = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if num_parts > (data.len() - 4) / 4 {
        return false;
    }
    let mut pos = 4;
    for i in 0..num_parts {
        if pos + 4 > data.len() {
            return false;
        }
        let part_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if part_len > data.len() - pos {
            return false;
        }
        if i > 0 {
            out.push(b".");
        }
        out.push(&data[pos..pos + part_len]);
        pos += part_len;
    }
    true
}

/// Parse property path from bytes
fn parse_path(data: &[u8]) -> Option<String> {
    // Proxy-Wasm の SDK（Rust・C++）と Envoy はパスの要素を NUL で区切る（`request\0path`）。
    // 従来の長さ付きの形式で読めなければ NUL 区切りとして読む（B-131）。
    parse_len_prefixed_path(data).or_else(|| {
        if data.is_empty() {
            return None;
        }
        let parts: Vec<String> = data
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        (!parts.is_empty()).then(|| parts.join("."))
    })
}

fn parse_len_prefixed_path(data: &[u8]) -> Option<String> {
    // Path is serialized as: [num_parts:4][part1_len:4][part1]...
    if data.len() < 4 {
        return None;
    }

    let num_parts = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    // 要素の数はゲストが渡すバイト列の値。各要素は少なくとも長さの 4 バイトを持つので、それより多い数は
    // 長さ付きの形式ではない（NUL 区切りの `plug...` を数として読むと 17 億になり、`with_capacity` が 41GB を
    // 確保しようとして OpenBSD で abort した。Linux は overcommit で気付かない。Wasm のモジュールが
    // ホストを落とせる確保なので、確保の前に断る）
    if num_parts > (data.len() - 4) / 4 {
        return None;
    }
    let mut parts = Vec::with_capacity(num_parts);
    let mut pos = 4;

    for _ in 0..num_parts {
        if pos + 4 > data.len() {
            return None;
        }
        let part_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + part_len > data.len() {
            return None;
        }
        let part = String::from_utf8_lossy(&data[pos..pos + part_len]).to_string();
        pos += part_len;

        parts.push(part);
    }

    Some(parts.join("."))
}

/// Add properties functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_get_property
    // B-20: wasm 側アロケータへ再入するため async ホスト関数
    linker.func_wrap_async(
        "env",
        "proxy_get_property",
        |mut caller: Caller<'_, HostState>,
         (path_ptr, path_size, return_value_ptr, return_value_size): (i32, i32, i32, i32)| {
            Box::new(async move {
                // Read path from WASM memory（B-178: 短いパスはスタックで組み立てる）
                let mut path = PropertyPath::new();
                {
                    let memory = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(mem)) => mem,
                        _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                    };
                    let data = memory.data(&caller);
                    let (start, len) = (path_ptr as usize, path_size as usize);
                    let raw = match start.checked_add(len).and_then(|end| data.get(start..end)) {
                        Some(r) => r,
                        None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                    };
                    if !parse_path_into(raw, &mut path) {
                        return PROXY_RESULT_PARSE_FAILURE;
                    }
                }

                let state = caller.data();
                let value = match get_property_value(state, path.as_str()) {
                    Some(v) => v,
                    None => return PROXY_RESULT_NOT_FOUND,
                };
                let value = value.as_bytes();

                // Allocate memory and write value
                let ptr = match allocate_wasm_memory(&mut caller, value.len()).await {
                    Some(p) => p,
                    None => return PROXY_RESULT_INTERNAL_FAILURE,
                };

                if !write_to_wasm(&mut caller, ptr, value) {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                // Write return values
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };

                let data = memory.data_mut(&mut caller);
                let ptr_offset = return_value_ptr as usize;
                let size_offset = return_value_size as usize;

                if ptr_offset + 4 > data.len() || size_offset + 4 > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
                data[size_offset..size_offset + 4]
                    .copy_from_slice(&(value.len() as i32).to_le_bytes());

                PROXY_RESULT_OK
            })
        },
    )?;

    // proxy_set_property
    linker.func_wrap(
        "env",
        "proxy_set_property",
        |mut caller: Caller<'_, HostState>,
         path_ptr: i32,
         path_size: i32,
         value_ptr: i32,
         value_size: i32|
         -> i32 {
            // Read path from WASM memory
            let path_data = match read_bytes(&mut caller, path_ptr, path_size) {
                Some(d) => d,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let path = match parse_path(&path_data) {
                Some(p) => p,
                None => return PROXY_RESULT_PARSE_FAILURE,
            };

            // Read value from WASM memory
            let value = if value_size > 0 {
                match read_bytes(&mut caller, value_ptr, value_size) {
                    Some(v) => v,
                    None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                }
            } else {
                Vec::new()
            };

            // Check capability
            let state = caller.data();
            if !state.http_ctx.capabilities.allow_property_write {
                ftlog::debug!("WASM: proxy_set_property - property write not allowed");
                return PROXY_RESULT_NOT_ALLOWED;
            }

            // Set the property
            let state = caller.data_mut();
            match path.as_str() {
                // Allow setting some request properties
                "request.path" => {
                    if let Ok(s) = String::from_utf8(value) {
                        state.http_ctx.request_path = std::sync::Arc::from(s);
                        return PROXY_RESULT_OK;
                    }
                    PROXY_RESULT_PARSE_FAILURE
                }
                "request.method" => {
                    if let Ok(s) = String::from_utf8(value) {
                        state.http_ctx.request_method = std::sync::Arc::from(s);
                        return PROXY_RESULT_OK;
                    }
                    PROXY_RESULT_PARSE_FAILURE
                }
                // For all other properties, store in custom_properties
                _ => {
                    state.http_ctx.custom_properties.insert(path.clone(), value);
                    ftlog::debug!("WASM: proxy_set_property '{}' set", path);
                    PROXY_RESULT_OK
                }
            }
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        get_property_value, parse_len_prefixed_path, parse_path, parse_path_into, PropertyPath,
        PropertyValue,
    };
    use crate::wasm::capabilities::ModuleCapabilities;
    use crate::wasm::context::{HostState, HttpContext};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn state_with(allowed: &[&str]) -> HostState {
        let caps = ModuleCapabilities {
            allowed_properties: allowed.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        HostState::new(HttpContext::new(1, caps))
    }

    /// B-178: `plugin_data.<name>` は読み込んだバイト列そのもの（`Arc`）を返し、複製しない
    #[test]
    fn plugin_data_is_shared_not_copied() {
        let data: Arc<[u8]> = Arc::from(&b"[1,2,3]"[..]);
        let mut map = HashMap::new();
        map.insert("dataset".to_string(), data.clone());
        let mut st = state_with(&["plugin_data.*", "request.path"]);
        st.http_ctx.plugin_data = Arc::new(map);
        st.http_ctx.request_path = Arc::from("/json/1?m=2");
        match get_property_value(&st, "plugin_data.dataset") {
            Some(PropertyValue::Shared(b)) => assert!(Arc::ptr_eq(&b, &data)),
            _ => panic!("plugin_data must be returned as the shared bytes"),
        }
        match get_property_value(&st, "request.path") {
            Some(PropertyValue::Str(p)) => {
                assert!(Arc::ptr_eq(&p, &st.http_ctx.request_path));
                assert_eq!(p.as_bytes(), b"/json/1?m=2");
            }
            _ => panic!("request.path must be returned as the shared string"),
        }
        assert!(get_property_value(&st, "plugin_data.missing").is_none());
        // 許可の無いプロパティは読めない
        assert!(get_property_value(&state_with(&["request.path"]), "plugin_data.dataset").is_none());
    }

    /// B-178: 確保しない版のパスの解析は従来の [`parse_path`] と同じ結果になる
    #[test]
    fn parse_path_into_matches_parse_path() {
        let mut lp = Vec::new();
        lp.extend_from_slice(&2u32.to_le_bytes());
        lp.extend_from_slice(&7u32.to_le_bytes());
        lp.extend_from_slice(b"request");
        lp.extend_from_slice(&4u32.to_le_bytes());
        lp.extend_from_slice(b"path");
        let long = [b"plugin_data\0".as_slice(), &[b'x'; 200]].concat();
        let bad = u32::MAX.to_le_bytes();
        let cases: [&[u8]; 8] = [
            &bad,
            b"plugin_root_id",
            b"request\0path",
            b"request\0path\0",
            b"plugin_data\0dataset",
            b"",
            &lp,
            &long,
        ];
        for c in cases {
            let mut p = PropertyPath::new();
            let ok = parse_path_into(c, &mut p);
            assert_eq!(ok.then(|| p.as_str().to_string()), parse_path(c), "{:?}", c);
        }
    }

    #[test]
    fn parse_path_accepts_sdk_nul_separated_paths() {
        // B-131: Proxy-Wasm の SDK（Rust・C++）は要素を NUL で区切る
        assert_eq!(
            parse_path(b"plugin_root_id").as_deref(),
            Some("plugin_root_id")
        );
        assert_eq!(
            parse_path(b"request\0path").as_deref(),
            Some("request.path")
        );
        assert_eq!(
            parse_path(b"request\0path\0").as_deref(),
            Some("request.path")
        );
        assert_eq!(parse_path(b""), None);
    }

    /// 要素の数にデータに収まらない値（ゲストが任意に渡せる）が来ても、その数で確保しない
    #[test]
    fn parse_len_prefixed_path_rejects_counts_larger_than_data() {
        assert_eq!(parse_len_prefixed_path(&u32::MAX.to_le_bytes()), None);
        let mut b = u32::MAX.to_le_bytes().to_vec();
        b.extend_from_slice(&[0u8; 16]);
        assert_eq!(parse_len_prefixed_path(&b), None);
    }

    #[test]
    fn parse_path_keeps_length_prefixed_paths() {
        let mut b = Vec::new();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&7u32.to_le_bytes());
        b.extend_from_slice(b"request");
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(b"path");
        assert_eq!(parse_path(&b).as_deref(), Some("request.path"));
    }
}
