//! 静的ファイルの配信（sendfile）
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// SendFile処理
// ====================
//
// kTLS + sendfile によるゼロコピー送信をサポートします。
//
// ## 通常の送信フロー（kTLS無効時）
//
// ファイル → ユーザー空間バッファ → TLS暗号化 → ネットワーク
// （2回のコピーが発生）
//
// ## ゼロコピー送信フロー（kTLS有効時）
//
// ファイル → カーネル空間でTLS暗号化 → NIC
// （ユーザー空間へのコピーなし）
//
// ### パフォーマンス効果
//
// - コンテキストスイッチの削減
// - メモリアクセスの削減（L3キャッシュミスの減少）
// - CPU使用率の低下（特に大きなファイル送信時）
//
// ### セキュリティ
//
// - ファイルの内容は変更されず、そのまま送信される
// - TLS暗号化はカーネル内で行われるため安全
// ====================

#[cfg_attr(not(feature = "wasm"), allow(unused_variables))]
pub(super) async fn handle_sendfile(
    mut tls_stream: ServerTls,
    base_path: &Path,
    is_dir: bool,
    index_filename: Option<&str>,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
    security: &SecurityConfig,
    // HEAD・Range・条件付き要求・Accept-Encoding（B-266・F-286・F-287）
    sreq: &crate::static_response::StaticRequest<'_>,
    // ルートの `[route.compression]`（B-269: HTTP/2・HTTP/3 と同じく静的配信も圧縮する）
    compression: &CompressionConfig,
    open_file_cache_config: Option<&cache::OpenFileCacheConfig>, // OpenFileCache設定（ルーティングごと）
    canonical_base: Option<&Path>, // base_path の canonical 形（F-145、config ロード時に一度だけ解決）
    wasm_modules: crate::wasm_plugin_config::WasmModules,
    static_file_cache_config: Option<&cache::StaticContentCacheRouteConfig>, // F-150: HTTP/1.1 ユーザー空間 TLS 経路向け静的コンテンツキャッシュ設定
) -> Option<(ServerTls, u16, u64, bool)> {
    // --- パス解決ロジック（Nginx風） ---
    //
    // 1. ファイル指定（is_dir=false）: 完全一致のみ
    //    例: prefix="/robots.txt", path="./www/robots.txt"
    //    - リクエスト "/robots.txt" → OK（ファイルを返す）
    //    - リクエスト "/robots.txt/extra" → 404（ファイルの下には入れない）
    //
    // 2. ディレクトリ指定（is_dir=true）: プレフィックス除去後のパスを結合
    //    例: prefix="/static/", path="./www/assets/"
    //    - リクエスト "/static/css/style.css" → "./www/assets/css/style.css"
    //    - リクエスト "/static/" → "./www/assets/{index_filename}" (デフォルト: index.html)

    // Cow<str>を使用してパス処理のアロケーションを最小化
    let path_str_raw = std::str::from_utf8(req_path).unwrap_or("/");
    // クエリ文字列を除去してファイルパス解決に使用するパスのみを取り出す
    let path_str = if let Some(qpos) = path_str_raw.find('?') {
        &path_str_raw[..qpos]
    } else {
        path_str_raw
    };
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");

    // プレフィックスを除去して「残りパス」を取得（借用のみ、アロケーションなし）
    let remainder: &str = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };

    // パストラバーサル防止（簡易チェック）
    if remainder.contains("..") {
        let err_buf = ERR_MSG_FORBIDDEN.to_vec();
        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
        return Some((tls_stream, 403, 0, true));
    }

    let full_path = if is_dir {
        // ケースA: ディレクトリへのルーティング（Alias動作）
        // config: path = "./www/static/"
        // req: /static/css/style.css → remainder: css/style.css
        // result: ./www/static/css/style.css
        let sub_path = remainder.trim_start_matches('/');
        let mut p = base_path.to_path_buf();
        if !sub_path.is_empty() {
            p.push(sub_path);
        }
        p
    } else {
        // ケースB: ファイルへの直接ルーティング（完全一致）
        // config: path = "./www/robots.txt"
        // req: /robots.txt → remainder: "" (OK)
        // req: /robots.txt/extra → remainder: "/extra" (NG → 404)

        let clean_remainder = remainder.trim_matches('/');
        if !clean_remainder.is_empty() {
            // ファイル指定なのにさらにパスが続いている場合は404
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));
        }
        base_path.to_path_buf()
    };

    // OpenFileCacheを使用してファイル情報を取得（キャッシュ優先）
    // これにより、canonicalize、metadata、mime_guessのシステムコールをキャッシュ
    // ルーティングごとの設定を考慮
    let file_info = match cache::get_file_info_with_config(&full_path, open_file_cache_config).await
    {
        Some(info) => info,
        None => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));
        }
    };

    // ディレクトリの場合はセキュリティチェック
    // F-145: base_path 側の canonicalize/metadata はリクエストごとに再実行せず、
    // config ロード時に一度だけ解決済みの canonical_base を使う
    // （`cache::sendfile_base_contains` 参照）。
    if is_dir
        && !cache::sendfile_base_contains(&file_info.canonical_path, canonical_base, base_path)
    {
        let err_buf = ERR_MSG_FORBIDDEN.to_vec();
        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
        return Some((tls_stream, 403, 0, true));
    }

    // ディレクトリの場合はインデックスファイルを試す。
    // `file_info` / `idx_info` は所有値なのでフィールドをムーブする（clone による
    // PathBuf / String の確保をしない）。open_file_cache が開いた fd を共有していれば
    // それを使い、open/close を省く（nginx の open_file_cache と同じ）。
    let (final_path, file_size, mime_type, shared_file, validators) = if !file_info.is_file {
        let filename = index_filename.unwrap_or("index.html");
        let index_path = file_info.canonical_path.join(filename);

        // インデックスファイルの情報をキャッシュから取得
        match cache::get_file_info_with_config(&index_path, open_file_cache_config).await {
            Some(idx_info) if idx_info.is_file => {
                let shared = idx_info.shared_file().cloned();
                (
                    idx_info.canonical_path,
                    idx_info.file_size,
                    idx_info.mime_type,
                    shared,
                    idx_info.validators,
                )
            }
            _ => {
                // インデックスファイルが存在しない場合は403 Forbidden
                let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 403, 0, true));
            }
        }
    } else {
        let shared = file_info.shared_file().cloned();
        (
            file_info.canonical_path,
            file_info.file_size,
            file_info.mime_type,
            shared,
            file_info.validators,
        )
    };

    // ファイルを開く（キャッシュが fd を保持していれば共有するだけで syscall 無し）
    let file = match shared_file {
        Some(f) => crate::runtime::io::File::from_shared(f),
        None => match OpenOptions::new().read(true).open(&final_path).await {
            Ok(f) => f,
            Err(_) => {
                // ファイルが開けない場合はキャッシュを無効化
                cache::invalidate_file_cache(&full_path);
                let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 404, 0, true));
            }
        },
    };

    // 返す応答を決める（HTTP/2・HTTP/3 と同じ関数。B-266・B-269・F-286・F-287）
    let plan =
        crate::static_response::plan(sreq, file_size, &mime_type, Some(&validators), compression);
    let response_status = plan.status();
    let vary = crate::static_response::compression_applies(compression, &mime_type, file_size);

    // 圧縮して返すときは本体を読んで圧縮する（静的コンテンツキャッシュ・圧縮結果のキャッシュが有効なら再利用）
    let compressed: Option<bytes::Bytes> = match plan {
        // HEAD は本文を送らないので、長さのためだけにファイルを読んで圧縮しない（Content-Length を付けない。B-304）
        crate::static_response::Plan::Full {
            encoding: Some(enc),
        } if !sreq.head => {
            let content_cfg =
                cache::effective_static_content_cache_config(static_file_cache_config);
            match load_file_bytes(&file, &final_path, file_size, &mime_type, &content_cfg).await {
                Some(data) => {
                    let level = cache::compressed::compression_level(enc, compression);
                    let ident = cache::compressed::file_ident(file_size, validators.mtime_secs);
                    Some(cache::compressed::get_or_compress(
                        &final_path,
                        enc,
                        level,
                        ident,
                        &content_cfg,
                        || crate::static_response::compress_body(&data, enc, compression),
                    ))
                }
                None => {
                    let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return Some((tls_stream, 404, 0, true));
                }
            }
        }
        _ => None,
    };

    // 本文の範囲（送る位置と長さ。304・416 は本文なし）
    let (transfer_offset, transfer_length) = match plan {
        crate::static_response::Plan::Partial { start, end } => (start as i64, end - start + 1),
        crate::static_response::Plan::Full { .. } => (
            0i64,
            compressed.as_ref().map_or(file_size, |c| c.len() as u64),
        ),
        _ => (0i64, 0u64),
    };

    let head_of_compressed = sreq.head
        && matches!(
            plan,
            crate::static_response::Plan::Full { encoding: Some(_) }
        );
    let header_buf = build_h1_static_head(
        &plan,
        file_size,
        (!head_of_compressed).then_some(transfer_length),
        &mime_type,
        Some(&validators),
        vary,
        security,
    );

    // WASMレスポンスヘッダーフィルタを適用（後段で Connection ヘッダーを追記するため mut）
    #[cfg(feature = "wasm")]
    let mut header_buf =
        match h1_wasm_response_head(header_buf, response_status, &wasm_modules).await {
            Ok(h) => h,
            Err(resp) => {
                return write_h1_wasm_local(&mut tls_stream, &resp, sreq.head)
                    .await
                    .map(|(st, n)| (tls_stream, st, n, client_wants_close));
            }
        };
    #[cfg(not(feature = "wasm"))]
    let mut header_buf = header_buf;

    // Connection ヘッダーを追加
    if client_wants_close {
        header_buf.extend_from_slice(b"Connection: close\r\n\r\n");
    } else {
        header_buf.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    }

    // HEAD・304・416 はヘッダだけ（B-266）
    if sreq.head || transfer_length == 0 {
        return match timeout(WRITE_TIMEOUT, tls_stream.write_all(header_buf)).await {
            Ok((Ok(_), _)) => Some((tls_stream, response_status, 0, client_wants_close)),
            _ => None,
        };
    }

    // 圧縮した本体はヘッダと一緒に 1 回で送る
    if let Some(body) = compressed {
        let len = body.len() as u64;
        return match timeout(
            WRITE_TIMEOUT,
            tls_stream.write_all_vectored(header_buf, body),
        )
        .await
        {
            Ok((Ok(_), _, _)) => Some((tls_stream, response_status, len, client_wants_close)),
            _ => None,
        };
    }

    // F-155: FreeBSD の平文（TLS 終端なし）接続に限り、レスポンスヘッダーと
    // ファイル本体を sf_hdtr 付き sendfile(2) 1 回で送る（write(2) + sendfile(2) の
    // 2 syscall を 1 syscall に統合する）。is_plain() は
    // handle_sendfile_userspace 側の既存 FreeBSD 分岐と同一条件（kTLS 有効時は
    // ServerTls::mode が KtlsTxOnly/KtlsFull になり is_plain() は false を返すため
    // 別途 kTLS 判定を足す必要はない。ktls_rustls.rs / simple_tls.rs の
    // is_plain() 実装を確認済み）。rustls ユーザー空間 TLS では絶対に通らない。
    #[cfg(target_os = "freebsd")]
    {
        if tls_stream.is_plain() && transfer_length > 0 {
            use crate::runtime::sendfile::sendfile_all_with_header;
            let out_fd = tls_stream.as_raw_fd();
            let in_fd = file.as_raw_fd();

            // F-155 Phase 6: sendfile(2) 呼び出しの間、TCP_NOPUSH でカーネルに
            // セグメントの早期送出を抑制させ、MSS 一杯までまとめて送出させる。
            // ガードが drop されると TCP_NOPUSH が解除され、残っているデータが
            // 即座にフラッシュされる（NoPushGuard の doc 参照）。取得（setsockopt）
            // 自体に失敗しても致命的ではないため None のまま続行する。
            //
            // ただしヘッダー + 本文が小さい応答（1 回の sendfile でまとめて送り切れる
            // サイズ）では早期送出の抑制に意味が無く、setsockopt 2 回（ON/OFF）が丸ごと
            // 無駄になる（FreeBSD の DTrace 実測で 2 syscall/req、プロファイル上位）。
            // v0.8.0 で 16KB → 128KB: ヘッダ + 本文を sf_hdtr 付きの sendfile(2) 1 回で出すので、1 回で送り切れる
            // 大きさ（54KB 等）では早期送出の抑制が要らない（FreeBSD aarch64 の h1_file_plain 54KB で +1〜1.5%）。
            const NOPUSH_MIN_BYTES: u64 = 128 * 1024;
            let nopush = if transfer_length + header_buf.len() as u64 > NOPUSH_MIN_BYTES {
                tls_stream.get_ref().nopush_guard()
            } else {
                None
            };

            let result = sendfile_all_with_header(
                out_fd,
                in_fd,
                transfer_offset,
                transfer_length as usize,
                &header_buf,
            )
            .await;

            // ガードを明示的に drop して TCP_NOPUSH を解除し、最後のセグメントを
            // 即座にフラッシュさせる（送信完了後に解除する必要があるため await の
            // 後でこのタイミングまで保持する）。
            drop(nopush);

            return match result {
                Ok(()) => Some((
                    tls_stream,
                    response_status,
                    transfer_length,
                    client_wants_close,
                )),
                Err(e) => {
                    error!("sendfile(2) with sf_hdtr error (FreeBSD plain): {}", e);
                    // ヘッダーが実際にどこまで送れたか（部分送信）は sf_hdtr 経路
                    // からは呼び出し側に分からないため、既存の FreeBSD sendfile
                    // エラー処理と同様に送信バイト数は 0 として報告し、接続は
                    // クローズ扱いにする。
                    Some((tls_stream, response_status, 0, true))
                }
            };
        }
    }

    // kTLS が有効な場合は sendfile によるゼロコピー送信を使用（ヘッダーを先に送る）
    #[cfg(veil_ktls)]
    {
        if tls_stream.is_ktls_send_enabled() {
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header_buf)).await;
            if !matches!(write_result, Ok((Ok(_), _))) {
                return None;
            }
            return handle_sendfile_zerocopy(
                tls_stream,
                &file,
                transfer_offset,
                transfer_length,
                client_wants_close,
                response_status,
            )
            .await;
        }
    }

    // kTLS が無効な場合は従来の read/write を使用（F-150: 静的コンテンツキャッシュ対象）。
    // ヘッダーは本文と一緒に送る（静的コンテンツキャッシュ命中時は 1 回の書き込み）。
    handle_sendfile_userspace(
        tls_stream,
        header_buf,
        &file,
        &final_path,
        file_size,
        &mime_type,
        static_file_cache_config,
        transfer_offset,
        transfer_length,
        client_wants_close,
        response_status,
    )
    .await
}

/// kTLS + sendfile によるゼロコピーファイル送信
///
/// kTLS が有効な場合に使用されます。
/// ファイルの内容をカーネル空間で直接 TLS 暗号化して送信します。
#[cfg(veil_ktls)]
pub(super) async fn handle_sendfile_zerocopy(
    tls_stream: ServerTls,
    file: &crate::runtime::io::File,
    transfer_offset: i64,
    transfer_length: u64,
    client_wants_close: bool,
    response_status: u16,
) -> Option<(ServerTls, u16, u64, bool)> {
    use std::os::unix::io::AsRawFd;

    let file_fd = file.as_raw_fd();
    let mut offset: i64 = transfer_offset;
    let target_end = transfer_offset + transfer_length as i64;
    let mut total_sent = 0u64;

    // sendfile を使用してファイルをゼロコピー送信
    // 設定に基づいてチャンクサイズを決定
    let chunk_size_config = {
        let config = CURRENT_CONFIG.load();
        match config.performance.chunk_size_mode {
            ChunkSizeMode::Dynamic => calculate_optimal_chunk_size(transfer_length),
            ChunkSizeMode::Manual => config.performance.manual_chunk_size,
        }
    };

    while offset < target_end {
        let remaining = (target_end - offset) as u64;
        let chunk_size = (remaining as usize).min(chunk_size_config);

        // sendfile 実行
        match tls_stream.sendfile(file_fd, &mut offset, chunk_size) {
            Ok(0) => {
                // EOF
                break;
            }
            Ok(n) => {
                total_sent += n as u64;
            }
            Err(e) => {
                // EAGAIN/EWOULDBLOCK の場合は再試行（非同期ソケットの場合）
                if e.kind() == io::ErrorKind::WouldBlock {
                    // writable を待ってから再試行
                    if (tls_stream.get_ref().writable().await).is_err() {
                        break;
                    }
                    continue;
                }
                error!("sendfile error: {}", e);
                break;
            }
        }
    }

    Some((tls_stream, response_status, total_sent, client_wants_close))
}

/// 従来の read/write によるファイル送信（ユーザー空間経由）
///
/// kTLS が無効な場合、または rustls 使用時に使用されます。
// clippy::too_many_arguments 許容理由: F-150 で静的コンテンツキャッシュのルックアップに
// 必要な値（解決済みパス・ファイルサイズ・決定済み MIME・ルート単位のキャッシュ設定）を
// 受け取るため引数が増えた。いずれも呼び出し元 `handle_sendfile` が既に算出済みの値であり、
// ここで構造体へまとめ直すとリクエストごとのアロケーション/ムーブが増えるため渡し切りにする。
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_sendfile_userspace(
    mut tls_stream: ServerTls,
    header_buf: Vec<u8>,
    file: &crate::runtime::io::File,
    file_path: &Path,
    file_size: u64,
    mime_type: &str,
    static_file_cache_config: Option<&cache::StaticContentCacheRouteConfig>,
    transfer_offset: i64,
    transfer_length: u64,
    client_wants_close: bool,
    response_status: u16,
) -> Option<(ServerTls, u16, u64, bool)> {
    // F-141: FreeBSD の平文（TLS 終端なし）接続に限り sendfile(2) によるゼロコピー
    // 送信を使う。TlsMode::Rustls（ユーザー空間 TLS）ではファイルの生バイトを暗号化
    // なしにソケットへ流すことになり平文漏洩になるため、is_plain() で厳密に判定する
    // （kTLS 有効時は既に handle_sendfile_zerocopy 側で処理済みでここには来ない）。
    //
    // F-155 で到達条件が変わった: 唯一の呼び出し元 handle_sendfile は、ヘッダー送信
    // より前に本分岐と全く同じ条件（is_plain() && transfer_length > 0）で
    // sf_hdtr 付き sendfile_all_with_header の高速経路へ既に分岐して return する
    // ため、通常のリクエスト処理では本分岐へは到達しない（tls_stream の TLS
    // モードは handle_sendfile 呼び出し中に変化しないため、2 箇所の条件は常に
    // 一致する）。それでも本分岐は削除せず残す: handle_sendfile_userspace は
    // 単体でも意味の通る「平文なら sendfile を使う」関数であり、将来 handle_sendfile
    // 以外の呼び出し元が増えた場合（例: 高速経路の条件を先に通さない別経路）や、
    // handle_sendfile 側の高速経路を将来外した場合に備えた安全網として機能する。
    #[cfg(target_os = "freebsd")]
    {
        if tls_stream.is_plain() && transfer_length > 0 {
            use crate::runtime::sendfile::sendfile_all_with_header;
            let out_fd = tls_stream.as_raw_fd();
            let in_fd = file.as_raw_fd();
            return match sendfile_all_with_header(
                out_fd,
                in_fd,
                transfer_offset,
                transfer_length as usize,
                &header_buf,
            )
            .await
            {
                Ok(()) => Some((
                    tls_stream,
                    response_status,
                    transfer_length,
                    client_wants_close,
                )),
                Err(e) => {
                    error!("sendfile(2) error (FreeBSD plain): {}", e);
                    // 部分送信済みバイト数は sendfile_all からは分からないため、ここでは
                    // 0 として報告する（アクセスログの送信バイト数は目安であり、接続は
                    // どのみち後続処理でクローズ扱いになるため実害はない）。
                    Some((tls_stream, response_status, 0, true))
                }
            };
        }
    }

    // F-150: kTLS 無効（ユーザー空間 rustls）経路の静的配信に静的コンテンツキャッシュを
    // 適用する。ここに到達した時点で、上の FreeBSD 平文 sendfile 分岐にも kTLS
    // ゼロコピー分岐（handle_sendfile_zerocopy、呼び出し元 handle_sendfile 参照）にも
    // 乗っていない＝暗号化コピーを伴うユーザー空間 TLS 経路か Linux の平文の経路であることが確定しており、
    // F-146 が「HTTP/1.1 は sendfile(2)/kTLS のゼロコピー経路だから対象外」としていた
    // 前提はここでは成立しない（詳細は F-150 チケット参照）。既定は enabled=false のため
    // 既定挙動は完全に不変。
    let content_cfg = cache::effective_static_content_cache_config(static_file_cache_config);
    if content_cfg.enabled && file_size <= content_cfg.max_file_size_bytes {
        // キャッシュはファイル全体を保持する（Range リクエストであっても部分内容だけを
        // 挿入すると、後続の別範囲・全体リクエストが正しく解決できなくなるため）。
        let cached = load_file_bytes(file, file_path, file_size, mime_type, &content_cfg).await;

        if let Some(data) = cached {
            let start = transfer_offset as usize;
            let end = start.saturating_add(transfer_length as usize);
            if end <= data.len() {
                // Bytes::slice は参照カウントのみ・コピーなし（Range リクエストの
                // 部分送出にもそのまま使える）。
                let slice = data.slice(start..end);
                // ヘッダーと本文を 1 回で送る（rustls は両方を送信キューへ積んでから
                // writev 1 回、平文は sendmsg 1 回。以前はヘッダーを先に別途書いていたため
                // 1 リクエストあたり書き込みが 2 回だった）。
                let write_result = timeout(
                    WRITE_TIMEOUT,
                    tls_stream.write_all_vectored(header_buf, slice),
                )
                .await;
                return match write_result {
                    Ok((Ok(_), _, _)) => Some((
                        tls_stream,
                        response_status,
                        transfer_length,
                        client_wants_close,
                    )),
                    _ => None,
                };
            }
            // data の長さがリクエスト範囲を満たさない（キャッシュ後にファイルが縮小した
            // 等の稀なレース）場合は安全側で下の通常経路へフォールスルーする。
        }
    }

    // 平文（TLS 終端なし）の Linux は本文を sendfile(2) でページキャッシュから直接送る（ユーザー空間へ
    // pread して send し直さない）。ヘッダは MSG_MORE で本文の最初のセグメントとまとめる。静的コンテンツキャッシュに
    // 載る本文は上のキャッシュから送る（検証子と同じ版の中身を返す。監視しないルートは有効期限まで古い中身のまま、
    // という他のプロトコルと同じ振る舞い。F-272）。rustls には通さない（ファイルの内容が暗号化されずに流れる）
    #[cfg(all(veil_ktls, target_os = "linux"))]
    let mut header_buf = Some(header_buf);
    #[cfg(all(veil_ktls, target_os = "linux"))]
    if tls_stream.is_plain() && transfer_length > 0 {
        match sendfile_plain(
            &tls_stream,
            header_buf.take(),
            file,
            transfer_offset,
            transfer_length,
        )
        .await
        {
            PlainSend::Sent(n) => {
                // 長さに届かなければ Content-Length と食い違うので接続を閉じる
                let close = client_wants_close || n < transfer_length;
                return Some((tls_stream, response_status, n, close));
            }
            PlainSend::HeaderFailed => return None,
            // ファイルが sendfile(2) に対応しない: ヘッダは送ったので本文だけ下の読み書きで送る
            PlainSend::Unsupported => {}
        }
    }
    #[cfg(all(veil_ktls, target_os = "linux"))]
    let header_buf = header_buf.unwrap_or_default();

    // ヘッダー送信（タイムアウト付き。平文で sendfile(2) に対応しないファイルはヘッダを送り済み）
    if !header_buf.is_empty() {
        let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    let mut total_sent = 0u64;
    let mut offset = transfer_offset as u64;
    let target_end = transfer_offset as u64 + transfer_length;

    while offset < target_end {
        let read_buf = buf_get();
        let (res, mut returned_buf) = file.read_at(read_buf, offset).await;

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => {
                // Range リクエストの場合、読み取りサイズを制限
                let remaining = (target_end - offset) as usize;
                n.min(remaining)
            }
            Err(e) => {
                buf_put(returned_buf);
                error!("File read error: {}", e);
                break;
            }
        };

        // SafeReadBuffer の有効長を設定して書き込み用Vecに変換
        returned_buf.set_valid_len(n);
        let write_buf = returned_buf.into_truncated();

        let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
                total_sent += n as u64;
                offset += n as u64;
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                break;
            }
            Err(_) => break,
        }
    }

    // 長さに届かずに終わったら Content-Length と食い違うので接続を閉じる（次の応答とずれる）
    let close = client_wants_close || total_sent < transfer_length;
    Some((tls_stream, response_status, total_sent, close))
}

/// 平文の sendfile(2) 1 回で送る上限。送り切るたびにワーカーへ譲る（54KB 等の小さい応答は 1 回で終わる）
#[cfg(all(veil_ktls, target_os = "linux"))]
const SENDFILE_PLAIN_CHUNK: usize = 1024 * 1024;

/// 平文の sendfile(2) の結果
#[cfg(all(veil_ktls, target_os = "linux"))]
enum PlainSend {
    /// ヘッダを送り、本文をこのバイト数だけ送った（途中で切れたら長さに届かない）
    Sent(u64),
    /// ヘッダを送れなかった（接続は使えない）
    HeaderFailed,
    /// ヘッダは送ったが、ファイルが sendfile(2) に対応しない（本文は 1 バイトも送っていない）
    Unsupported,
}

/// 平文の接続へヘッダ（`MSG_MORE`）と本文（`sendfile(2)`）を送る（Linux）。
///
/// 受理したソケットは非ブロッキング（`SOCK_NONBLOCK`）なので、送信バッファが一杯なら `EAGAIN` で戻り、書き込み可能を
/// 待って続きから送る（`sendfile(2)` が `offset` を進める）。ファイルの読み込みはページキャッシュから行う（kTLS の
/// `handle_sendfile_zerocopy` と同じ）。`sendfile(2)` に `MSG_NOSIGNAL` は付けられないが、相手が閉じたときの SIGPIPE は
/// プロセスで無視している（Rust の実行時が `main` の前に `SIG_IGN` にする）ので `EPIPE` として返る。
#[cfg(all(veil_ktls, target_os = "linux"))]
async fn sendfile_plain(
    tls_stream: &ServerTls,
    header: Option<Vec<u8>>,
    file: &crate::runtime::io::File,
    transfer_offset: i64,
    transfer_length: u64,
) -> PlainSend {
    use std::os::unix::io::AsRawFd;

    let sock = tls_stream.get_ref();
    if let Some(h) = header {
        if !matches!(
            timeout(WRITE_TIMEOUT, async_raw_send_more_all(sock, &h)).await,
            Ok(Ok(()))
        ) {
            return PlainSend::HeaderFailed;
        }
    }

    let out_fd = sock.as_raw_fd();
    let in_fd = file.as_raw_fd();
    let mut offset = transfer_offset;
    let end = transfer_offset + transfer_length as i64;
    let mut sent = 0u64;
    while offset < end {
        let count = ((end - offset) as usize).min(SENDFILE_PLAIN_CHUNK);
        // SAFETY: 両 fd は呼び出しの間有効で、offset は有効なスタック変数（カーネルが進める）。
        let r = unsafe { libc::sendfile(out_fd, in_fd, &mut offset, count) };
        if r > 0 {
            sent += r as u64;
            // 速く受け取る相手へ大きいファイルを送る間も同じワーカーの他の接続を止めない（1 回の上限まで送れたら譲る）
            if r as usize == SENDFILE_PLAIN_CHUNK && offset < end {
                crate::runtime::yield_now().await;
            }
            continue;
        }
        if r == 0 {
            // ファイルが縮んだ（長さに届かないまま終わる）
            break;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::WouldBlock => {
                // B-17 と同じ: 受け取らないクライアントで永久に待たない
                if !matches!(timeout(WRITE_TIMEOUT, sock.writable()).await, Ok(Ok(()))) {
                    break;
                }
            }
            io::ErrorKind::Interrupted => {}
            _ if sent == 0
                && matches!(
                    e.raw_os_error(),
                    Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                debug!("sendfile(2) is not supported for this file: {}", e);
                return PlainSend::Unsupported;
            }
            _ => {
                debug!("sendfile(2) error (plain): {}", e);
                break;
            }
        }
    }
    PlainSend::Sent(sent)
}

/// ファイル全体を `Bytes` で得る（静的コンテンツキャッシュが有効ならキャッシュから、無ければ開いている fd から 1 回の
/// `read_exact_at` で読み、キャッシュに入れる）。読めなければ `None`。
///
/// `cache::get_or_load` は `std::fs::read(path)` で **開き直す**ため使えない（FreeBSD capability mode = `cap_enter` 下では
/// 絶対パス open が禁止されている、F-123）。既に開いている fd を使えば open/close も増えない。
pub(super) async fn load_file_bytes(
    file: &crate::runtime::io::File,
    file_path: &Path,
    file_size: u64,
    mime_type: &str,
    content_cfg: &cache::StaticContentCacheConfig,
) -> Option<bytes::Bytes> {
    let cacheable = content_cfg.enabled && file_size <= content_cfg.max_file_size_bytes;
    if cacheable {
        if let Some(data) = cache::get_cached_content_cache(file_path, content_cfg).await {
            return Some(data);
        }
    }
    // `Vec::with_capacity` はゼロ初期化を伴わず、`read_exact_at` が生ポインタへ直接書き込む（`IoBufMut` 実装参照）。
    let read_buf: Vec<u8> = Vec::with_capacity(file_size as usize);
    let (res, returned_buf) = file.read_exact_at(read_buf, 0).await;
    match res {
        Ok(n) if n as u64 == file_size => {
            // Vec<u8> -> Bytes はヒープ確保をそのまま引き継ぐだけで memcpy を伴わない
            let data = bytes::Bytes::from(returned_buf);
            if cacheable {
                // mime_guess の再計算を避けるため、呼び出し元が決定済みの MIME タイプを使い回す
                cache::insert_content_cache_bytes(
                    file_path,
                    data.clone(),
                    Arc::from(mime_type),
                    content_cfg,
                );
            }
            Some(data)
        }
        _ => None,
    }
}

/// 名前を `Content-Type` の形（先頭と `-` の後を大文字）で HTTP/1.1 のヘッダ行として足す
fn push_h1_header(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    // `emit_headers` の付ける名前は決まっているので表記をそのまま写す（1 バイトずつ大文字にしない）
    let title: &[u8] = match name {
        b"content-type" => b"Content-Type",
        b"accept-ranges" => b"Accept-Ranges",
        b"content-range" => b"Content-Range",
        b"etag" => b"ETag",
        b"last-modified" => b"Last-Modified",
        b"content-encoding" => b"Content-Encoding",
        b"vary" => b"Vary",
        _ => b"",
    };
    if title.is_empty() {
        let mut upper = true;
        for &b in name {
            out.push(if upper { b.to_ascii_uppercase() } else { b });
            upper = b == b'-';
        }
    } else {
        out.extend_from_slice(title);
    }
    out.extend_from_slice(b": ");
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

/// 静的配信（`type = "File"`・`mode = "memory"`）の HTTP/1.1 の応答ヘッダを、ステータス行から `Connection` の直前まで
/// 組み立てる（`static_response::emit_headers` を HTTP/2・HTTP/3 と共有。B-266・B-269・F-286・F-287）。
///
/// `content_length` は送る本文の長さ（HEAD でも GET と同じ値。304 では付けない）。
pub(super) fn build_h1_static_head(
    plan: &crate::static_response::Plan,
    size: u64,
    content_length: Option<u64>,
    mime: &str,
    validators: Option<&crate::static_response::Validators>,
    vary: bool,
    security: &SecurityConfig,
) -> Vec<u8> {
    use crate::static_response::Plan;
    let mut h = Vec::with_capacity(HEADER_BUF_CAPACITY);
    h.extend_from_slice(match plan {
        Plan::Full { .. } => b"HTTP/1.1 200 OK\r\n".as_slice(),
        Plan::Partial { .. } => b"HTTP/1.1 206 Partial Content\r\n",
        Plan::NotModified { .. } => b"HTTP/1.1 304 Not Modified\r\n",
        Plan::Unsatisfiable => b"HTTP/1.1 416 Range Not Satisfiable\r\n",
    });
    let removes = !security.remove_response_headers.is_empty();
    crate::static_response::emit_headers(plan, size, mime, validators, vary, |n, v| {
        if !removes || !security.removes_response_header(n) {
            push_h1_header(&mut h, n, v.as_slice());
        }
    });
    // `[server] server_header_enabled`（B-271。HTTP/2・HTTP/3 と同じ）
    if !removes || !security.removes_response_header(b"server") {
        crate::pool::append_server_header_line(&mut h);
    }
    // 長さが分からない（HEAD の圧縮表現で、圧縮していない）ときは付けない（B-304）
    if let (false, Some(len)) = (matches!(plan, Plan::NotModified { .. }), content_length) {
        h.extend_from_slice(b"Content-Length: ");
        let mut num = itoa::Buffer::new();
        h.extend_from_slice(num.format(len).as_bytes());
        h.extend_from_slice(b"\r\n");
    }
    // 追加レスポンスヘッダー（セキュリティヘッダーなど）
    for (header_name, header_value) in &security.add_response_headers {
        h.extend_from_slice(header_name.as_bytes());
        h.extend_from_slice(b": ");
        h.extend_from_slice(header_value.as_bytes());
        h.extend_from_slice(b"\r\n");
    }
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
    crate::route_cors::append_h1_lines(&mut h, security);
    // F-94: HTTP/3 広告（Alt-Svc）
    crate::pool::append_own_alt_svc_line(&mut h, security);
    h
}

/// HTTP/1.1 の応答ヘッダ（ステータス行からヘッダの行まで。空行は含めない）に Wasm の `on_http_response_headers` を
/// かける。モジュールが無ければそのまま返す。
///
/// ステータス行と本文の区切り（`Content-Length`・`Transfer-Encoding`・`Connection`）は元のまま残し、モジュールには
/// 変えさせない（送る本文と食い違うと接続が壊れる。以前の静的配信は 206・304 でも 200 に作り直し、`Content-Length` を
/// 二重に付けていた。B-268）。
///
/// モジュールがローカルレスポンスを返した（`send_local_response`、または `failure_policy` で拒否した）ら `Err` で返す。
/// 呼び出し元は元の応答の代わりにそれを送る（B-300。以前は捨てて元の応答を送っていた）。
#[cfg(feature = "wasm")]
pub(super) async fn h1_wasm_response_head(
    head: Vec<u8>,
    status: u16,
    wasm_modules: &crate::wasm_plugin_config::WasmModules,
) -> Result<Vec<u8>, crate::wasm::LocalResponse> {
    if wasm_modules.is_empty() {
        return Ok(head);
    }
    let config = CURRENT_CONFIG.load();
    let Some(ref wasm_engine) = config.wasm_filter_engine else {
        return Ok(head);
    };
    let status_end = match head.windows(2).position(|w| w == b"\r\n") {
        Some(i) => i + 2,
        None => return Ok(head),
    };
    let is_framing = |name: &[u8]| {
        name.eq_ignore_ascii_case(b"content-length")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"connection")
    };
    let mut current: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(16);
    for line in head[status_end..].split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        current.push((
            line[..colon].to_vec(),
            line[colon + 1..].trim_ascii().to_vec(),
        ));
    }
    let result = wasm_engine
        .clone()
        .on_response_headers_with_modules_async(wasm_modules.clone(), status, current, true)
        .await;
    let modified = match result {
        crate::wasm::FilterResult::Continue { headers, .. } => headers,
        crate::wasm::FilterResult::LocalResponse(resp) => return Err(resp),
    };
    let mut out = Vec::with_capacity(head.len() + 64);
    out.extend_from_slice(&head[..status_end]);
    for (name, value) in modified.iter().filter(|(n, _)| !is_framing(n)) {
        out.extend_from_slice(name);
        out.extend_from_slice(b": ");
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
    // 元の区切りの行をそのまま戻す
    for line in head[status_end..].split_inclusive(|&b| b == b'\n') {
        let name_end = line.iter().position(|&b| b == b':').unwrap_or(0);
        if name_end > 0 && is_framing(&line[..name_end]) {
            out.extend_from_slice(line);
        }
    }
    Ok(out)
}

/// 応答ヘッダの段のローカルレスポンス（B-300）。`wasm` feature が無ければ作られない
#[cfg(feature = "wasm")]
pub(super) type HeadLocal = crate::wasm::LocalResponse;
#[cfg(not(feature = "wasm"))]
pub(super) type HeadLocal = std::convert::Infallible;

/// 応答ヘッダの段のローカルレスポンスを、元の応答の代わりに送る（B-300）。元の応答の本文は送らないので、
/// 接続は続けられる（`client_wants_close` に従う）。戻り値は `(ステータス, 書いた長さ)`、書けなければ `None`。
pub(super) async fn write_h1_wasm_local<S: crate::runtime::io::AsyncWriteRentExt>(
    stream: &mut S,
    resp: &HeadLocal,
    head_method: bool,
) -> Option<(u16, u64)> {
    #[cfg(feature = "wasm")]
    {
        let bytes = super::h1::wasm_local_response_h1(resp, head_method);
        let len = bytes.len() as u64;
        match timeout(WRITE_TIMEOUT, stream.write_all(bytes)).await {
            Ok((Ok(_), _)) => Some((resp.status_code, len)),
            _ => None,
        }
    }
    #[cfg(not(feature = "wasm"))]
    {
        let _ = (stream, head_method);
        match *resp {}
    }
}
