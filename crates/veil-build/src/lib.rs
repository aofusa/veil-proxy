//! `.proto` から veil の gRPC サーバ（F-197）のコードを生成する。`build.rs` で使う:
//!
//! ```no_run
//! // build.rs
//! fn main() -> std::io::Result<()> {
//!     veil_build::compile_protos(&["proto/helloworld.proto"], &["proto"])
//! }
//! ```
//!
//! メッセージは prost-build がそのまま生成し、サービスごとに `<service>_server` モジュールを足す:
//!
//! - `trait <Service>`: メソッドごとに `fn x(&self, Request<_>) -> impl Future<Output = Result<Response<_>, Status>>`
//!   （実装側は `async fn` で書ける。Future に `Send` は要求しない。veil のワーカー上で動く）。
//!   サーバ/双方向ストリーミングの応答は `veil::api::grpc::BoxStream<T>`、受信ストリームは `Streaming<T>`。
//! - `fn add_<service>(app, svc) -> App`: 全メソッドを `App::grpc_*` で登録する（静的ディスパッチ）。
//! - `SERVICE_NAME`・`METHODS`（`/<package>.<Service>/<Method>` の一覧）。
//! - `ProstCodec`: prost のメッセージを符号化・復号する `veil::api::grpc::Codec` の実装（F-208）。
//! - `FILE_DESCRIPTOR_SET`: コンパイルした `.proto`（と import 先）の `FileDescriptorSet`。
//!   `App::grpc_reflection(&[FILE_DESCRIPTOR_SET])` でサーバリフレクションに使う（F-209）。
//!   veil 本体は protobuf の実装に依存しないので、prost との橋渡しは生成コードが持つ。
//!
//! 生成コードは tokio・tonic に依存しない（`veil::api::grpc` と prost のみ）。利用者のクレートに `prost` の依存が要る
//! （prost-build 0.13 が生成するコードに合わせて `prost = "0.13"`）。

use prost_build::{Method, Service, ServiceGenerator};
use std::fmt::Write as _;
use std::io;
use std::path::Path;

/// 既定の設定で `.proto` をコンパイルする（出力は `OUT_DIR/<package>.rs`、`veil::include_proto!` で取り込む）。
pub fn compile_protos(
    protos: &[impl AsRef<Path>],
    includes: &[impl AsRef<Path>],
) -> io::Result<()> {
    configure().compile_protos(protos, includes)
}

/// 設定を変えてコンパイルするためのビルダ。
pub fn configure() -> Builder {
    Builder {
        config: prost_build::Config::new(),
        veil_path: "::veil".to_string(),
    }
}

/// コード生成の設定。
pub struct Builder {
    config: prost_build::Config,
    veil_path: String,
}

impl Builder {
    /// veil クレートのパス（`veil = { package = "...", ... }` で別名にしているとき）。既定 `::veil`。
    pub fn veil_path(mut self, path: impl Into<String>) -> Builder {
        self.veil_path = path.into();
        self
    }

    /// prost-build の設定（型属性・出力先等）。
    pub fn config_mut(&mut self) -> &mut prost_build::Config {
        &mut self.config
    }

    pub fn compile_protos(
        mut self,
        protos: &[impl AsRef<Path>],
        includes: &[impl AsRef<Path>],
    ) -> io::Result<()> {
        // F-209: サーバリフレクション用に FileDescriptorSet を OUT_DIR に書き、生成コードから include_bytes! する。
        // build.rs の外（OUT_DIR が無い）ではリフレクション用の定数を出さない。
        let descriptor = std::env::var_os("OUT_DIR").map(|out| {
            let stem = protos
                .first()
                .and_then(|p| p.as_ref().file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            Path::new(&out).join(format!("veil_fds_{}.bin", stem))
        });
        if let Some(path) = &descriptor {
            self.config.file_descriptor_set_path(path);
        }
        self.config
            .service_generator(Box::new(VeilServiceGenerator {
                veil: self.veil_path.clone(),
                descriptor: descriptor.map(|p| p.to_string_lossy().into_owned()),
            }));
        self.config.compile_protos(protos, includes)
    }
}

/// サービス定義からサーバ側のコードを書く。
pub struct VeilServiceGenerator {
    veil: String,
    /// `FileDescriptorSet` のファイル（生成コードの `FILE_DESCRIPTOR_SET` が取り込む）
    descriptor: Option<String>,
}

impl VeilServiceGenerator {
    pub fn new(veil_path: impl Into<String>) -> VeilServiceGenerator {
        VeilServiceGenerator {
            veil: veil_path.into(),
            descriptor: None,
        }
    }
}

fn to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// prost が返す型パス（パッケージのモジュールからの相対）を生成モジュール内から見たパスにする。
fn rel(ty: &str) -> String {
    if ty.starts_with("::") || ty.starts_with("crate::") {
        ty.to_string()
    } else {
        format!("super::{}", ty)
    }
}

fn doc(lines: &[String], indent: &str, buf: &mut String) {
    for l in lines {
        let _ = writeln!(buf, "{}///{}", indent, l);
    }
}

fn path_of(service: &Service, method: &Method) -> String {
    if service.package.is_empty() {
        format!("/{}/{}", service.proto_name, method.proto_name)
    } else {
        format!(
            "/{}.{}/{}",
            service.package, service.proto_name, method.proto_name
        )
    }
}

impl ServiceGenerator for VeilServiceGenerator {
    fn generate(&mut self, service: Service, buf: &mut String) {
        let v = &self.veil;
        let g = format!("{}::api::grpc", v);
        let module = format!("{}_server", to_snake(&service.name));
        let full_name = if service.package.is_empty() {
            service.proto_name.clone()
        } else {
            format!("{}.{}", service.package, service.proto_name)
        };

        let _ = writeln!(buf, "/// `{}` のサーバ（veil-build が生成）。", full_name);
        let _ = writeln!(buf, "pub mod {} {{", module);
        let _ = writeln!(buf, "    #![allow(clippy::all, unused_imports)]");
        let _ = writeln!(buf, "    pub const SERVICE_NAME: &str = \"{}\";", full_name);
        let _ = write!(buf, "    pub const METHODS: &[&str] = &[");
        for m in &service.methods {
            let _ = write!(buf, "\"{}\", ", path_of(&service, m));
        }
        let _ = writeln!(buf, "];");
        if let Some(d) = &self.descriptor {
            let _ = writeln!(
                buf,
                "    /// コンパイルした `.proto` の `FileDescriptorSet`（`App::grpc_reflection` に渡す）。"
            );
            let _ = writeln!(
                buf,
                "    pub const FILE_DESCRIPTOR_SET: &[u8] = ::core::include_bytes!({:?});",
                d
            );
        }

        // codec（F-208）: このクレートにローカルな型なので、prost の任意のメッセージ型に veil の trait を実装できる
        let _ = writeln!(
            buf,
            "    /// prost のメッセージの符号化と復号（veil-build が生成）。"
        );
        let _ = writeln!(buf, "    #[derive(Clone, Copy, Debug, Default)]");
        let _ = writeln!(buf, "    pub struct ProstCodec;");
        let _ = writeln!(
            buf,
            "    impl<T: ::prost::Message + ::core::default::Default + 'static> {g}::Codec<T> for ProstCodec {{"
        );
        let _ = writeln!(
            buf,
            "        fn encode(&self, message: &T, out: &mut {g}::BytesMut) -> ::core::result::Result<(), {g}::Status> {{"
        );
        let _ = writeln!(
            buf,
            "            ::prost::Message::encode(message, out).map_err(|e| {g}::Status::internal(e.to_string()))"
        );
        let _ = writeln!(buf, "        }}");
        let _ = writeln!(
            buf,
            "        fn decode(&self, buf: {g}::Bytes) -> ::core::result::Result<T, {g}::Status> {{"
        );
        let _ = writeln!(
            buf,
            "            <T as ::prost::Message>::decode(buf).map_err(|e| {g}::Status::internal(::std::format!(\"failed to decode message: {{}}\", e)))"
        );
        let _ = writeln!(buf, "        }}");
        let _ = writeln!(
            buf,
            "        fn encoded_len(&self, message: &T) -> usize {{ ::prost::Message::encoded_len(message) }}"
        );
        let _ = writeln!(buf, "    }}");

        // トレイト
        doc(&service.comments.leading, "    ", buf);
        let _ = writeln!(
            buf,
            "    pub trait {}: Send + Sync + 'static {{",
            service.name
        );
        for m in &service.methods {
            let input = rel(&m.input_type);
            let output = rel(&m.output_type);
            let req = if m.client_streaming {
                format!("{g}::Request<{g}::Streaming<{input}>>")
            } else {
                format!("{g}::Request<{input}>")
            };
            let resp = if m.server_streaming {
                format!("{g}::Response<{g}::BoxStream<{output}>>")
            } else {
                format!("{g}::Response<{output}>")
            };
            doc(&m.comments.leading, "        ", buf);
            let _ = writeln!(
                buf,
                "        fn {}(&self, request: {}) -> impl ::core::future::Future<Output = ::core::result::Result<{}, {g}::Status>>;",
                m.name, req, resp
            );
        }
        let _ = writeln!(buf, "    }}");

        // 登録関数
        let _ = writeln!(
            buf,
            "    /// `{}` の全メソッドを `App` に登録する。",
            full_name
        );
        let _ = writeln!(
            buf,
            "    pub fn add_{}<S: {}>(app: {v}::api::app::App, service: S) -> {v}::api::app::App {{",
            to_snake(&service.name),
            service.name
        );
        let _ = writeln!(buf, "        let service = ::std::sync::Arc::new(service);");
        let _ = writeln!(buf, "        let app = app;");
        for m in &service.methods {
            let input = rel(&m.input_type);
            let reg = match (m.client_streaming, m.server_streaming) {
                (false, false) => "grpc_unary",
                (false, true) => "grpc_server_streaming",
                (true, false) => "grpc_client_streaming",
                (true, true) => "grpc_bidi",
            };
            let req = if m.client_streaming {
                format!("{g}::Request<{g}::Streaming<{input}>>")
            } else {
                format!("{g}::Request<{input}>")
            };
            let _ = writeln!(buf, "        let app = {{");
            let _ = writeln!(buf, "            let s = service.clone();");
            let _ = writeln!(
                buf,
                "            app.{reg}(\"{}\", ProstCodec, move |req: {req}| {{ let s = s.clone(); async move {{ s.{}(req).await }} }})",
                path_of(&service, m),
                m.name
            );
            let _ = writeln!(buf, "        }};");
        }
        let _ = writeln!(buf, "        app");
        let _ = writeln!(buf, "    }}");
        let _ = writeln!(buf, "}}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case() {
        assert_eq!(to_snake("SayHello"), "say_hello");
        assert_eq!(to_snake("Greeter"), "greeter");
        assert_eq!(to_snake("VeilDiscoveryService"), "veil_discovery_service");
    }

    #[test]
    fn relative_type_paths() {
        assert_eq!(rel("HelloRequest"), "super::HelloRequest");
        assert_eq!(rel("super::other::X"), "super::super::other::X");
        assert_eq!(rel("::prost_types::Empty"), "::prost_types::Empty");
    }
}
