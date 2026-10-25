//! Istio の WasmPlugin を veil の Proxy-Wasm で動かす（F-222 #7）。
//!
//! - istiod は WasmPlugin を HCM の HTTP フィルタ（`config_discovery`）として配り、本体は ECDS
//!   （`TypedExtensionConfig`）で届く。pilot-agent の xDS プロキシが `vm_config.code.remote`（OCI・HTTP）を
//!   取得してローカルのファイルに書き換え、取得に失敗したら（`failStrategy` に従い）拒否の RBAC か何もしない
//!   フィルタに差し替える。veil はローカルのファイルだけを読む。
//! - プラグインごとに Proxy-Wasm のエンジン（`crate::wasm::FilterEngine`）を 1 つ作り、ECDS の更新で作り直す。
//!   要求ヘッダ（`proxy_on_request_headers`）と応答ヘッダ（`proxy_on_response_headers`）の段で呼ぶ。
//! - 要求ごとの費用は Wasm フィルタがある HCM だけに掛かる（無い HCM は `Ran` が空のまま）。

use super::l7::{ConnCtx, LocalReply, ReqParts};
use crate::wasm::{FilterEngine, FilterResult, ModuleCapabilities, ModuleConfig, WasmConfig};
use crate::wasm_plugin_config::{ModuleRef, PluginConfiguration};
use bytes::Bytes;
use std::sync::Arc;

/// 1 つの WasmPlugin（ECDS の 1 リソース）。
pub struct MeshWasm {
    pub name: String,
    engine: Arc<FilterEngine>,
    modules: Arc<Vec<ModuleRef>>,
}

impl std::fmt::Debug for MeshWasm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshWasm")
            .field("name", &self.name)
            .finish()
    }
}

/// 要求の段で通った Wasm フィルタ（応答の段で逆順に呼ぶ）。
pub type Ran = Vec<Arc<MeshWasm>>;

/// Istio のプラグインに渡す権限（Envoy の Wasm には権限の制限が無い。外部呼び出しはメッシュのクラスタに
/// 解決できないので許さない）。
fn capabilities() -> ModuleCapabilities {
    let mut c = crate::wasm::CapabilityPreset::Extended.to_capabilities();
    c.allow_http_calls = false;
    c
}

/// プラグインを読み込む（ECDS の適用時。xDS のスレッドで呼ぶ）。
pub fn load(
    name: &str,
    path: &str,
    root_id: &str,
    configuration: Vec<u8>,
    fail_closed: bool,
) -> Result<MeshWasm, String> {
    let module = ModuleConfig {
        name: name.to_string(),
        path: path.to_string(),
        configuration: PluginConfiguration::Raw(
            String::from_utf8_lossy(&configuration).into_owned(),
        ),
        capabilities: capabilities(),
        ..Default::default()
    };
    // Envoy の Wasm にはメモリの上限が無い（C++ SDK の emscripten 製は最小で 16MB を要求する）。
    // プーリングアロケータは 1 スロットごとに仮想アドレスを予約するだけなので、上限を大きめにする
    let mut defaults = crate::wasm::WasmDefaults::default();
    defaults.pooling.max_memory_size = 256 * 1024 * 1024;
    let cfg = WasmConfig {
        enabled: true,
        defaults,
        modules: vec![module],
        interpreter: false,
    };
    let mut engine =
        FilterEngine::new(&cfg).map_err(|e| format!("wasm plugin {}: {:#}", name, e))?;
    // 空の root_id もそのまま渡す（C++ SDK は空の名前で登録したファクトリを引く）
    engine.set_root_id(name, root_id);
    engine.set_fail_closed(fail_closed);
    Ok(MeshWasm {
        name: name.to_string(),
        engine: Arc::new(engine),
        modules: Arc::new(vec![ModuleRef::from_name(name)]),
    })
}

/// 要求ヘッダの段。ヘッダ（疑似ヘッダを含む）の変更を `rq` に戻す。プラグインのローカル応答は `Err`。
pub async fn on_request(
    w: &MeshWasm,
    ctx: &ConnCtx,
    rq: &mut ReqParts<'_>,
) -> Result<(), LocalReply> {
    let mut hv: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(rq.headers.len() + 4);
    hv.push((b":method".to_vec(), rq.method.to_vec()));
    hv.push((b":path".to_vec(), rq.path.to_vec()));
    hv.push((b":authority".to_vec(), rq.authority.to_vec()));
    hv.push((b":scheme".to_vec(), rq.scheme.to_vec()));
    for (k, v) in rq.headers.iter() {
        hv.push((k.to_vec(), v.to_vec()));
    }
    let path: Arc<str> = Arc::from(String::from_utf8_lossy(rq.path).as_ref());
    let method: Arc<str> = Arc::from(String::from_utf8_lossy(rq.method).as_ref());
    let ip: Arc<str> = Arc::from(ctx.peer.ip().to_string());
    let r = w
        .engine
        .clone()
        .on_request_headers_with_modules_async(w.modules.clone(), path, method, hv, ip, rq.eos)
        .await;
    match r {
        FilterResult::Continue { headers, .. } => {
            rq.headers.clear();
            for (k, v) in headers {
                match k.as_slice() {
                    b":method" => *rq.method = Bytes::from(v),
                    b":path" => *rq.path = Bytes::from(v),
                    b":authority" => *rq.authority = Bytes::from(v),
                    b":scheme" => {}
                    _ => rq.headers.push((Bytes::from(k), Bytes::from(v))),
                }
            }
            Ok(())
        }
        FilterResult::Pause => {
            ftlog::debug!(
                "[mesh] wasm plugin {} paused the request; continuing",
                w.name
            );
            Ok(())
        }
        FilterResult::LocalResponse(resp) => Err(local_reply(resp)),
    }
}

/// 応答ヘッダの段（逆順）。プラグインのローカル応答は応答の段では使わない（ヘッダの変更だけ反映する）。
pub async fn on_response(ran: &Ran, status: u16, headers: &mut Vec<(Bytes, Bytes)>) {
    for w in ran.iter().rev() {
        let hv: Vec<(Vec<u8>, Vec<u8>)> = headers
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
        let r = w
            .engine
            .clone()
            .on_response_headers_with_modules_async(w.modules.clone(), status, hv, false)
            .await;
        if let FilterResult::Continue { headers: h, .. } = r {
            headers.clear();
            headers.extend(
                h.into_iter()
                    .filter(|(k, _)| !k.starts_with(b":"))
                    .map(|(k, v)| (Bytes::from(k), Bytes::from(v))),
            );
        }
    }
}

fn local_reply(resp: crate::wasm::LocalResponse) -> LocalReply {
    let mut lr = LocalReply::new(resp.status_code, super::httpcore::ResponseFlag::None);
    lr.body = Bytes::from(resp.body);
    lr.headers = resp
        .headers
        .into_iter()
        .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
        .collect();
    lr
}
