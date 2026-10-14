//! gRPC サーバリフレクション（`grpc.reflection.v1` と `v1alpha`、F-209 #3）。`App::grpc_reflection` で登録する。
//!
//! grpcurl・Postman・grpcui 等が `.proto` 無しにサービスの一覧と型を調べられる。型の情報は `veil-build` が
//! 生成コードに埋め込む `FileDescriptorSet`（`<service>_server::FILE_DESCRIPTOR_SET`）から引く。
//! veil は protobuf の実装に依存しないので、記述子は必要なフィールド（名前・パッケージ・依存・メッセージ・列挙・
//! サービス・拡張）だけを `wire` で読み、ファイルの記述子はバイト列のまま返す。
//!
//! - `list_services`: 登録された gRPC のサービス（記述子に無いもの・ヘルスチェック・リフレクション自身も含む）。
//! - `file_by_filename`・`file_containing_symbol`・`file_containing_extension`: そのファイルと、依存する
//!   ファイル（推移的に）。
//! - `all_extension_numbers_of_type`: 拡張の番号。
//! - 見つからないときは `error_response`（`NOT_FOUND`）。ストリームは続く。

use super::{wire, Codec, Request, Response, Status, Streaming};
use crate::api::app::App;
use bytes::{Bytes, BytesMut};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

/// 登録された gRPC のサービス名（`grpc_route` が登録時に足す。起動時だけ書かれる）。
static SERVICES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

pub(crate) fn record_service(name: &str) {
    SERVICES.lock().unwrap().insert(name.to_string());
}

// ===================== 記述子の索引 =====================

struct File {
    raw: Bytes,
    deps: Vec<String>,
}

#[derive(Default)]
struct Index {
    files: Vec<File>,
    by_name: HashMap<String, usize>,
    symbols: HashMap<String, usize>,
    extensions: HashMap<(String, i32), usize>,
}

fn full(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", package, name)
    }
}

impl Index {
    fn add_set(&mut self, set: &[u8]) -> Result<(), Status> {
        let mut r = wire::Reader::new(set);
        while let Some((f, w)) = r.next_key()? {
            if f == 1 && w == wire::LEN {
                let raw = r.read_bytes()?;
                self.add_file(Bytes::copy_from_slice(raw))?;
            } else {
                r.skip(w)?;
            }
        }
        Ok(())
    }

    fn add_file(&mut self, raw: Bytes) -> Result<(), Status> {
        let mut name = String::new();
        let mut package = String::new();
        let mut deps = Vec::new();
        let mut messages = Vec::new();
        let mut enums = Vec::new();
        let mut services = Vec::new();
        let mut exts = Vec::new();
        let mut r = wire::Reader::new(&raw);
        while let Some((f, w)) = r.next_key()? {
            match (f, w) {
                (1, wire::LEN) => name = r.read_string()?,
                (2, wire::LEN) => package = r.read_string()?,
                (3, wire::LEN) => deps.push(r.read_string()?),
                (4, wire::LEN) => messages.push(r.read_bytes()?),
                (5, wire::LEN) => enums.push(r.read_bytes()?),
                (6, wire::LEN) => services.push(r.read_bytes()?),
                (7, wire::LEN) => exts.push(r.read_bytes()?),
                _ => r.skip(w)?,
            }
        }
        if self.by_name.contains_key(&name) {
            // 複数の FileDescriptorSet が同じ import（google/protobuf/*.proto 等）を含む
            return Ok(());
        }
        let idx = self.files.len();
        for m in messages {
            self.add_message(m, &package, idx)?;
        }
        for e in enums {
            let n = name_of(e)?;
            self.symbols.insert(full(&package, &n), idx);
        }
        for s in services {
            let (svc, methods) = service_of(s)?;
            let svc = full(&package, &svc);
            for m in methods {
                self.symbols.insert(format!("{}.{}", svc, m), idx);
            }
            self.symbols.insert(svc, idx);
        }
        for e in exts {
            self.add_extension(e, &package, idx)?;
        }
        self.by_name.insert(name, idx);
        self.files.push(File {
            raw: raw.clone(),
            deps,
        });
        Ok(())
    }

    fn add_message(&mut self, m: &[u8], scope: &str, idx: usize) -> Result<(), Status> {
        let mut name = String::new();
        let mut nested = Vec::new();
        let mut enums = Vec::new();
        let mut exts = Vec::new();
        let mut r = wire::Reader::new(m);
        while let Some((f, w)) = r.next_key()? {
            match (f, w) {
                (1, wire::LEN) => name = r.read_string()?,
                (3, wire::LEN) => nested.push(r.read_bytes()?),
                (4, wire::LEN) => enums.push(r.read_bytes()?),
                (6, wire::LEN) => exts.push(r.read_bytes()?),
                _ => r.skip(w)?,
            }
        }
        let me = full(scope, &name);
        for n in nested {
            self.add_message(n, &me, idx)?;
        }
        for e in enums {
            let n = name_of(e)?;
            self.symbols.insert(full(&me, &n), idx);
        }
        for e in exts {
            self.add_extension(e, &me, idx)?;
        }
        self.symbols.insert(me, idx);
        Ok(())
    }

    fn add_extension(&mut self, field: &[u8], scope: &str, idx: usize) -> Result<(), Status> {
        let mut name = String::new();
        let mut extendee = String::new();
        let mut number = 0i32;
        let mut r = wire::Reader::new(field);
        while let Some((f, w)) = r.next_key()? {
            match (f, w) {
                (1, wire::LEN) => name = r.read_string()?,
                (2, wire::LEN) => extendee = r.read_string()?,
                (3, wire::VARINT) => number = r.read_varint()? as i32,
                _ => r.skip(w)?,
            }
        }
        self.symbols.insert(full(scope, &name), idx);
        let extendee = extendee.trim_start_matches('.').to_string();
        self.extensions.insert((extendee, number), idx);
        Ok(())
    }

    /// ファイルと推移的な依存（重複無し、要求したファイルが先頭）。
    fn with_deps(&self, idx: usize) -> Vec<Bytes> {
        let mut seen = vec![false; self.files.len()];
        let mut order = Vec::new();
        let mut stack = vec![idx];
        while let Some(i) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            order.push(self.files[i].raw.clone());
            for d in self.files[i].deps.iter().rev() {
                if let Some(&j) = self.by_name.get(d) {
                    stack.push(j);
                }
            }
        }
        order
    }
}

fn name_of(m: &[u8]) -> Result<String, Status> {
    let mut r = wire::Reader::new(m);
    while let Some((f, w)) = r.next_key()? {
        if f == 1 && w == wire::LEN {
            return r.read_string();
        }
        r.skip(w)?;
    }
    Ok(String::new())
}

fn service_of(s: &[u8]) -> Result<(String, Vec<String>), Status> {
    let mut name = String::new();
    let mut methods = Vec::new();
    let mut r = wire::Reader::new(s);
    while let Some((f, w)) = r.next_key()? {
        match (f, w) {
            (1, wire::LEN) => name = r.read_string()?,
            (2, wire::LEN) => methods.push(name_of(r.read_bytes()?)?),
            _ => r.skip(w)?,
        }
    }
    Ok((name, methods))
}

// ===================== メッセージ =====================

/// `ServerReflectionRequest`
#[derive(Clone, Debug, Default)]
pub struct ReflectionRequest {
    host: String,
    kind: RequestKind,
    /// 符号化されたままの要求（応答の `original_request` に返す）
    raw: Bytes,
}

#[derive(Clone, Debug, Default)]
enum RequestKind {
    FileByFilename(String),
    Symbol(String),
    Extension(String, i32),
    AllExtensionNumbers(String),
    ListServices,
    #[default]
    Unknown,
}

/// `ServerReflectionResponse`
#[derive(Clone, Debug)]
pub struct ReflectionResponse {
    host: String,
    original: Bytes,
    kind: ResponseKind,
}

#[derive(Clone, Debug)]
enum ResponseKind {
    Files(Vec<Bytes>),
    ExtensionNumbers(String, Vec<i32>),
    Services(Vec<String>),
    Error(i32, String),
}

/// リフレクションのメッセージの手書きの符号化（veil は protobuf の実装に依存しない。F-208）。
#[derive(Clone, Copy, Debug, Default)]
pub struct ReflectionCodec;

impl Codec<ReflectionRequest> for ReflectionCodec {
    fn encode(&self, m: &ReflectionRequest, out: &mut BytesMut) -> Result<(), Status> {
        out.extend_from_slice(&m.raw);
        Ok(())
    }
    fn decode(&self, buf: Bytes) -> Result<ReflectionRequest, Status> {
        let mut req = ReflectionRequest {
            raw: buf.clone(),
            ..Default::default()
        };
        let mut r = wire::Reader::new(&buf);
        while let Some((f, w)) = r.next_key()? {
            match (f, w) {
                (1, wire::LEN) => req.host = r.read_string()?,
                (3, wire::LEN) => req.kind = RequestKind::FileByFilename(r.read_string()?),
                (4, wire::LEN) => req.kind = RequestKind::Symbol(r.read_string()?),
                (5, wire::LEN) => {
                    let mut ty = String::new();
                    let mut num = 0;
                    let mut e = wire::Reader::new(r.read_bytes()?);
                    while let Some((f, w)) = e.next_key()? {
                        match (f, w) {
                            (1, wire::LEN) => ty = e.read_string()?,
                            (2, wire::VARINT) => num = e.read_varint()? as i32,
                            _ => e.skip(w)?,
                        }
                    }
                    req.kind = RequestKind::Extension(ty, num);
                }
                (6, wire::LEN) => req.kind = RequestKind::AllExtensionNumbers(r.read_string()?),
                (7, wire::LEN) => {
                    r.read_bytes()?;
                    req.kind = RequestKind::ListServices;
                }
                _ => r.skip(w)?,
            }
        }
        Ok(req)
    }
}

impl Codec<ReflectionResponse> for ReflectionCodec {
    fn encode(&self, m: &ReflectionResponse, out: &mut BytesMut) -> Result<(), Status> {
        wire::put_bytes_field(out, 1, m.host.as_bytes());
        wire::put_message_field(out, 2, &m.original);
        let mut inner = BytesMut::new();
        let field = match &m.kind {
            ResponseKind::Files(files) => {
                for f in files {
                    wire::put_message_field(&mut inner, 1, f);
                }
                4
            }
            ResponseKind::ExtensionNumbers(ty, nums) => {
                wire::put_bytes_field(&mut inner, 1, ty.as_bytes());
                wire::put_packed_int32_field(&mut inner, 2, nums);
                5
            }
            ResponseKind::Services(names) => {
                for n in names {
                    let mut s = BytesMut::new();
                    wire::put_bytes_field(&mut s, 1, n.as_bytes());
                    wire::put_message_field(&mut inner, 1, &s);
                }
                6
            }
            ResponseKind::Error(code, msg) => {
                wire::put_int32_field(&mut inner, 1, *code);
                wire::put_bytes_field(&mut inner, 2, msg.as_bytes());
                7
            }
        };
        wire::put_message_field(out, field, &inner);
        Ok(())
    }
    fn decode(&self, _buf: Bytes) -> Result<ReflectionResponse, Status> {
        Err(Status::unimplemented(
            "reflection responses are not decoded",
        ))
    }
}

fn answer(index: &Index, req: ReflectionRequest) -> ReflectionResponse {
    let not_found = |what: String| ResponseKind::Error(super::Code::NotFound.as_u8() as i32, what);
    let kind = match &req.kind {
        RequestKind::ListServices => {
            let mut names: BTreeSet<String> = SERVICES.lock().unwrap().clone();
            // 記述子にあって登録されていないサービスは載せない（呼べないため）
            names.retain(|n| !n.is_empty());
            ResponseKind::Services(names.into_iter().collect())
        }
        RequestKind::FileByFilename(name) => match index.by_name.get(name) {
            Some(&i) => ResponseKind::Files(index.with_deps(i)),
            None => not_found(format!("file {} not found", name)),
        },
        RequestKind::Symbol(sym) => match index.symbols.get(sym.trim_start_matches('.')) {
            Some(&i) => ResponseKind::Files(index.with_deps(i)),
            None => not_found(format!("symbol {} not found", sym)),
        },
        RequestKind::Extension(ty, num) => {
            match index
                .extensions
                .get(&(ty.trim_start_matches('.').to_string(), *num))
            {
                Some(&i) => ResponseKind::Files(index.with_deps(i)),
                None => not_found(format!("extension {} of {} not found", num, ty)),
            }
        }
        RequestKind::AllExtensionNumbers(ty) => {
            let ty = ty.trim_start_matches('.');
            if !index.symbols.contains_key(ty) {
                not_found(format!("type {} not found", ty))
            } else {
                let mut nums: Vec<i32> = index
                    .extensions
                    .keys()
                    .filter(|(t, _)| t == ty)
                    .map(|(_, n)| *n)
                    .collect();
                nums.sort_unstable();
                ResponseKind::ExtensionNumbers(ty.to_string(), nums)
            }
        }
        RequestKind::Unknown => ResponseKind::Error(
            super::Code::Unimplemented.as_u8() as i32,
            "unsupported reflection request".into(),
        ),
    };
    ReflectionResponse {
        host: req.host,
        original: req.raw,
        kind,
    }
}

impl App {
    /// サーバリフレクション（`grpc.reflection.v1` と `v1alpha` の `ServerReflectionInfo`）を登録する（F-209）。
    /// `descriptor_sets` は `veil-build` の生成コードの `<service>_server::FILE_DESCRIPTOR_SET`
    /// （`FileDescriptorSet` を符号化したバイト列。複数のパッケージを渡せる）。
    ///
    /// 記述子が壊れていたら panic する（起動時に一度だけ。生成コードの定数を渡す前提）。
    pub fn grpc_reflection(self, descriptor_sets: &[&[u8]]) -> App {
        let mut index = Index::default();
        for set in descriptor_sets {
            if let Err(e) = index.add_set(set) {
                panic!(
                    "grpc_reflection: invalid FileDescriptorSet: {}",
                    e.message()
                );
            }
        }
        let index = Arc::new(index);
        let mut app = self;
        for path in [
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
            "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo",
        ] {
            let index = index.clone();
            app = app.grpc_bidi(
                path,
                ReflectionCodec,
                move |req: Request<Streaming<ReflectionRequest>>| {
                    let index = index.clone();
                    async move {
                        use futures::StreamExt;
                        let stream = req.into_inner().map(move |r| r.map(|r| answer(&index, r)));
                        Ok(Response::new(stream))
                    }
                },
            );
        }
        app
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(fields: &[(u32, &[u8])]) -> Vec<u8> {
        let mut b = BytesMut::new();
        for (f, v) in fields {
            wire::put_message_field(&mut b, *f, v);
        }
        b.to_vec()
    }

    /// `pkg/a.proto`（package pkg、message Outer { message Inner; enum E }、service Svc { rpc Do }、
    /// extend Outer = 100）が `dep.proto` に依存する記述子。
    fn descriptor_set() -> Vec<u8> {
        let inner = msg(&[(1, b"Inner")]);
        let e = msg(&[(1, b"E")]);
        let mut ext = BytesMut::new();
        wire::put_bytes_field(&mut ext, 1, b"ext_field");
        wire::put_bytes_field(&mut ext, 2, b".pkg.Outer");
        wire::put_int32_field(&mut ext, 3, 100);
        let outer = msg(&[(1, b"Outer"), (3, &inner), (4, &e)]);
        let method = msg(&[(1, b"Do")]);
        let svc = msg(&[(1, b"Svc"), (2, &method)]);
        let file_a = msg(&[
            (1, b"pkg/a.proto"),
            (2, b"pkg"),
            (3, b"dep.proto"),
            (4, &outer),
            (6, &svc),
            (7, &ext),
        ]);
        let file_dep = msg(&[(1, b"dep.proto"), (4, &msg(&[(1, b"Dep")]))]);
        msg(&[(1, &file_dep), (1, &file_a)])
    }

    fn request(field: u32, v: &[u8]) -> ReflectionRequest {
        let mut b = BytesMut::new();
        wire::put_bytes_field(&mut b, 1, b"localhost");
        wire::put_message_field(&mut b, field, v);
        ReflectionCodec.decode(b.freeze()).unwrap()
    }

    fn files_of(r: &ReflectionResponse) -> Vec<String> {
        match &r.kind {
            ResponseKind::Files(f) => f
                .iter()
                .map(|raw| {
                    let mut rd = wire::Reader::new(raw);
                    let (_, _) = rd.next_key().unwrap().unwrap();
                    rd.read_string().unwrap()
                })
                .collect(),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn symbols_files_and_extensions() {
        let mut index = Index::default();
        let set = descriptor_set();
        index.add_set(&set).unwrap();
        // 同じ記述子をもう一度足しても重複しない
        index.add_set(&set).unwrap();
        assert_eq!(index.files.len(), 2);
        for sym in [
            "pkg.Outer",
            "pkg.Outer.Inner",
            "pkg.Outer.E",
            "pkg.Svc",
            "pkg.Svc.Do",
            ".pkg.Svc",
        ] {
            let r = answer(&index, request(4, sym.as_bytes()));
            assert_eq!(files_of(&r), ["pkg/a.proto", "dep.proto"], "{}", sym);
        }
        let r = answer(&index, request(3, b"dep.proto"));
        assert_eq!(files_of(&r), ["dep.proto"]);
        let r = answer(&index, request(4, b"pkg.Nope"));
        assert!(matches!(r.kind, ResponseKind::Error(5, _)));
        let mut ext = BytesMut::new();
        wire::put_bytes_field(&mut ext, 1, b"pkg.Outer");
        wire::put_int32_field(&mut ext, 2, 100);
        let r = answer(&index, request(5, &ext));
        assert_eq!(files_of(&r), ["pkg/a.proto", "dep.proto"]);
        let r = answer(&index, request(6, b"pkg.Outer"));
        assert!(
            matches!(&r.kind, ResponseKind::ExtensionNumbers(t, n) if t == "pkg.Outer" && n == &[100])
        );
        // 応答は host と元の要求を返す
        assert_eq!(r.host, "localhost");
        let mut out = BytesMut::new();
        ReflectionCodec.encode(&r, &mut out).unwrap();
        assert!(!out.is_empty());
    }

    #[test]
    fn list_services_reports_registered_services() {
        let _app = App::new().grpc_reflection(&[&descriptor_set()]);
        record_service("pkg.Svc");
        let r = answer(&Index::default(), request(7, b"*"));
        match r.kind {
            ResponseKind::Services(s) => {
                assert!(s.contains(&"pkg.Svc".to_string()), "{:?}", s);
                assert!(s.contains(&"grpc.reflection.v1.ServerReflection".to_string()));
            }
            other => panic!("{:?}", other),
        }
    }
}
