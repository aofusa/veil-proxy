//! ダーティ接続の集合とタイマーの最小ヒープ・受信と送信（F-151）
//!
//! F-233 で `src/http3_server.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// F-151: ダーティ接続集合 + タイマー最小ヒープ（イベント駆動化）
// ====================

/// タイマーヒープの pop が来ない場合のフォールバック値であり、かつ `select` の sleep 時間の
/// 上限クランプにも使う（従来のタイムアウト粒度 100ms から挙動を変えない）。
pub(super) const H3_DEFAULT_TIMER: Duration = Duration::from_millis(100);

/// 接続 ID の共有ハンドル（`ConnKey` = `Rc<ConnectionId<'static>>`）。ダーティキュー/
/// タイマーヒープ/送出対象リストはすべてこの型を使い、`clone()` を `Rc::clone`
/// （参照カウント +1 のみ）に落として `ConnectionId` 本体のヒープ確保を避ける
/// （F-151 レビュー修正。定義本体は [`crate::http3_stream::ConnKey`]）。
pub(super) type ConnKey = crate::http3_stream::ConnKey;

/// タイマーヒープ（`BinaryHeap<Reverse<TimerKey>>`）のエントリ。
///
/// `quiche::ConnectionId` は `Ord`/`PartialOrd` を実装していないため `(Instant,
/// ConnectionId)` タプルをそのまま `BinaryHeap` の要素にはできない。本型は `Instant` のみで
/// 順序付けし（同着時の順序は問わない。タイマー期限の到来判定にしか使わないため cid の
/// 大小関係は無関係）、`ConnKey` は識別子として運ぶだけにする。
pub(super) struct TimerKey(pub(super) Instant, pub(super) ConnKey);

impl PartialEq for TimerKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for TimerKey {}
impl PartialOrd for TimerKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// ヒープ先頭の期限から次の `select` sleep 時間を算出する（quiche/`Http3Handler` 非依存の
/// 純粋関数。単体テスト対象）。
///
/// - **`has_dirty = true`（ダーティ集合に未処理の接続が残っている）→ 常に 0。**
///   前イテレーションで `did_work` により再投入された接続がある状態で sleep してしまうと、
///   ストリーミング中のレスポンスがチャンクごとに最大 `H3_DEFAULT_TIMER` 停止し得る
///   （レイテンシ退行。F-151 レビュー修正）。
/// - ヒープが空（`top = None`）→ 既定 100ms。
/// - 先頭期限が既に過去（`top <= now`）→ 0（即座に処理すべきタイマーがある）。
/// - 先頭期限が未来 → その差分。ただし上限を 100ms にクランプし、従来のタイムアウト粒度
///   から外れないようにする。
pub(super) fn next_sleep_duration(top: Option<Instant>, now: Instant, has_dirty: bool) -> Duration {
    if has_dirty {
        return Duration::ZERO;
    }
    match top {
        None => H3_DEFAULT_TIMER,
        Some(dl) if dl <= now => Duration::ZERO,
        Some(dl) => (dl - now).min(H3_DEFAULT_TIMER),
    }
}

/// タイマーヒープの遅延削除判定（quiche/`Http3Handler` 非依存の純粋関数。単体テスト対象）。
///
/// pop したエントリの期限 `popped_deadline` が、そのハンドラの現在の期限
/// （`Http3Handler::timer_deadline` 相当）と一致する場合のみ有効。期限が更新された後の
/// 古いエントリは `false` を返し、呼び出し側は無視して捨てる。
pub(super) fn timer_entry_is_current(
    popped_deadline: Instant,
    current_deadline: Option<Instant>,
) -> bool {
    current_deadline == Some(popped_deadline)
}

/// ダーティフラグ管理の核（quiche/`Http3Handler` 非依存。単体テスト対象）。
///
/// 既にダーティ（`*dirty == true`）なら何もしない（多重登録防止）。そうでなければ
/// ダーティにしてキューへ push する。戻り値は「実際に push したか」。
pub(super) fn mark_dirty_flag<T: Clone>(dirty: &mut bool, queue: &mut VecDeque<T>, id: &T) -> bool {
    if *dirty {
        false
    } else {
        *dirty = true;
        queue.push_back(id.clone());
        true
    }
}

/// 接続をダーティ集合へ登録する（多重登録防止）。`Http3Handler::dirty` が `false` のときだけ
/// `true` にしてキューへ push する。
///
/// 呼び出し元が既に対象 `Http3Handler` を可変借用している場合はこの関数を使わず、
/// `mark_dirty_flag(&mut handler.dirty, queue, key)` を直接呼ぶ（`conns` の二重借用を避ける
/// ため。`process_datagram_segments` の受信直後の呼び出し箇所を参照）。
pub(super) fn mark_dirty(conns: &mut ConnMapInner, queue: &mut VecDeque<ConnKey>, key: &ConnKey) {
    // `ConnKey` = `Rc<ConnectionId>` を `&**key` で deref し、HashMap キー（素の
    // `ConnectionId<'static>`）としてルックアップする。
    if let Some(h) = conns.get_mut(&**key) {
        mark_dirty_flag(&mut h.dirty, queue, key);
    }
}

/// コネクション処理直後にタイマーヒープへ次回期限を登録する（遅延削除方式）。
///
/// `conn.timeout_instant()` が `None`（アイドル/未確立等）の場合は `H3_DEFAULT_TIMER` を
/// フォールバックとして使う。
///
/// ダーティ接続ごと・イテレーションごとに呼ばれるため時刻を読まない: quiche の
/// `timeout()` は内部で `Instant::now()` を読んで残り時間に変換するので、従来の
/// `Instant::now() + conn.timeout()` は 1 回あたり時刻読み取り 2 回だった
/// （FreeBSD aarch64 の HTTP/3 プロファイルで `__vdso_gettc` が最上位）。
/// 期限の絶対時刻 `timeout_instant()` をそのまま使えば同じ値になる。
pub(super) fn schedule_timer(
    handler: &mut Http3Handler,
    key: &ConnKey,
    timers: &mut BinaryHeap<Reverse<TimerKey>>,
) {
    let mut deadline = match handler.conn.timeout_instant() {
        Some(t) => t,
        None => Instant::now() + H3_DEFAULT_TIMER,
    };
    // B-222: ハンドシェイク中はその期限にも起きる
    if let Some(hs) = &handler.handshake {
        deadline = deadline.min(hs.deadline());
    }
    // ヒープに同じか早いエントリが既にあれば積まない（そのエントリが満了したときに本当の期限を確かめ直す。
    // `expire_due_timers`）。以前は接続を処理するたびに積み、期限が延びた古いエントリをヒープに溜めては
    // 捨てていた（FreeBSD の h3_file 54KB で expire_due_timers・HashMap::get_mut が上位。v0.8.0）。
    if handler
        .timer_deadline
        .is_some_and(|queued| queued <= deadline)
    {
        return;
    }
    handler.timer_deadline = Some(deadline);
    timers.push(Reverse(TimerKey(deadline, key.clone())));
}

/// タイマーヒープから期限到来分だけ pop して `on_timeout` を呼び、ダーティ化する（F-151）。
///
/// 全接続の `conn.timeout()` の最小値を求める走査と、期限が来ていない接続まで
/// `on_timeout` する走査（従来方式）を廃止する。遅延削除方式: pop したエントリの期限が
/// 現在の `timer_deadline` と一致しなければ（期限が更新済みの古いエントリ）無視して捨てる。
pub(super) fn expire_due_timers(
    conns: &mut ConnMapInner,
    timers: &mut BinaryHeap<Reverse<TimerKey>>,
    dirty_queue: &mut VecDeque<ConnKey>,
    now: Instant,
) {
    while let Some(top) = timers.peek() {
        if top.0 .0 > now {
            break;
        }
        // peek で存在確認済みなので pop は必ず成功する。
        let Reverse(TimerKey(dl, key)) = timers.pop().expect("peek succeeded");

        let Some(handler) = conns.get_mut(&*key) else {
            continue; // 接続は既に削除済み。
        };
        if !timer_entry_is_current(dl, handler.timer_deadline) {
            continue; // 期限が更新済みの古いエントリ（遅延削除）。
        }
        handler.timer_deadline = None;
        // B-222: 期限までに確立しなかった接続は捨てる（偽装した送信元の接続を max_idle_timeout まで残さない）
        if handler
            .handshake
            .as_ref()
            .is_some_and(|hs| hs.deadline() <= now)
        {
            debug!("[HTTP/3] Handshake timed out from {}", handler.peer_addr);
            conns.remove(&*key);
            continue;
        }
        // 積んだ後に期限が延びていたら（`schedule_timer` は早くなるときだけ積み直す）、本当の期限で積み直す
        if let Some(mut actual) = handler.conn.timeout_instant() {
            if let Some(hs) = &handler.handshake {
                actual = actual.min(hs.deadline());
            }
            if actual > now {
                handler.timer_deadline = Some(actual);
                timers.push(Reverse(TimerKey(actual, key)));
                continue;
            }
        }
        handler.conn.set_time_hint(Some(now));
        handler.conn.on_timeout();
        handler.conn.set_time_hint(None);
        if handler.conn.is_closed() {
            debug!("[HTTP/3] Connection closed (timeout)");
            conns.remove(&*key);
            continue;
        }
        mark_dirty(conns, dirty_queue, &key);
    }
}

/// バックエンド通知キュー（`ConnWaker` が積む cid）を drain してダーティ化する（F-151）。
///
/// per-connection 化により「どの接続が進んだか」を追跡できるため、全接続をダーティ化する
/// フォールバックは不要（本関数はキューに積まれた cid だけを処理する）。
///
/// F-161: 要素は `(cid, queued フラグ)`。**pop → flag=false → mark_dirty** の順で処理する。
/// flag を先に false に戻してから mark_dirty するため、この間（mark_dirty 実行中も含む）に
/// バックエンドタスクが新たに notify() しても「flag は false → push される」ため
/// 取りこぼされず、次の drain で確実に処理される。逆に flag を mark_dirty の後で false に
/// 戻す順序にすると、mark_dirty 実行後・flag=false 前に発生した notify() が
/// 「flag はまだ true → push されない」まま消えてしまう（取りこぼし）。
pub(super) fn drain_wake_queue(
    wake_queue: &crate::http3_stream::WakeQueue,
    conns: &mut ConnMapInner,
    dirty_queue: &mut VecDeque<ConnKey>,
) {
    // borrow は drain 中だけ（`mark_dirty` は `wake_queue` に触れないため二重借用にならない）。
    let mut q = wake_queue.borrow_mut();
    while let Some((key, queued)) = q.pop_front() {
        queued.set(false);
        mark_dirty(conns, dirty_queue, &key);
    }
}

/// F-115 第2段: 受信した 1 データグラム（GRO 集約含む）を quiche へ供給する。
///
/// select 経由の最初の 1 通と recvmmsg 経由の各通で共用する（挙動を両経路で一致させるため
/// クロージャではなく専用 fn へ抽出）。`data` は受信済みデータグラム全体（長さ = 受信バイト数）で、
/// GRO 適用時は `gro_segment_size` 境界で複数 QUIC パケットへ分割される。
///
/// ゼロコピー: `data` のスライスを `quiche::Header::from_slice` と `conn.recv` に直接渡し、
/// 中間 Vec へのコピーを一切行わない。
///
/// F-45: GRO バッチはカーネルが**同一フロー**のデータグラムを集約したものなので、直前セグメントと
/// 同じ DCID なら新規接続判定（contains_key + Initial 検査）をスキップし、per-segment の
/// オーバーヘッドをルックアップ 1 回に抑える（`prev_cid` 最適化）。quiche の `recv` API は
/// 1 データグラム単位のため呼び出し自体は per-segment。
///
/// F-151: そのコネクションに `conn.recv()` した接続（新規コネクション生成時も含む）は
/// 必ずダーティ化する。これがダーティ化の契機の 1 つ（他の契機はタイマー期限到来・
/// バックエンド起床キュー・前回パスの仕事継続）。
///
/// **タイマーの再登録はここでは行わない**（レビュー修正）。本関数は 1 データグラムごとに
/// 呼ばれるホットパスであり、GRO で束ねられた 54KB のレスポンス相当では 1 リクエストあたり
/// 40 回以上呼ばれ得る。ここで `schedule_timer` を呼ぶと呼び出しのたびに `ConnKey` の
/// ヒープ確保（`Rc::new` 相当）は避けられても `BinaryHeap::push` が積み重なり、F-151 で
/// 削減したかった固定費を上回りかねない。recv した接続は必ずダーティ化されるため、
/// **同一イテレーション内のダーティ処理ループ**（呼び出し元のメインループ）で
/// `schedule_timer` が接続ごとに高々 1 回だけ呼ばれ、タイマーの再登録は漏れなく行われる。
/// B-94: サーバ接続 ID の導出鍵（プロセス全体で 1 個）。
///
/// SO_REUSEPORT で同じ 4-tuple は同じワーカーへ届くが、ワーカー間で鍵を分ける理由は無いので
/// プロセスで共有する（`cbpf` 振り分け等で 4-tuple とワーカーの対応が変わっても同じ SCID になる）。
pub(super) static SERVER_CID_KEY: once_cell::sync::Lazy<hmac::Key> =
    once_cell::sync::Lazy::new(|| {
        let mut seed = [0u8; 32];
        SystemRandom::new()
            .fill(&mut seed)
            .expect("HTTP/3: failed to generate connection ID key");
        hmac::Key::new(hmac::HMAC_SHA256, &seed)
    });

/// B-94: クライアントの元 DCID からサーバ接続 ID（`MAX_CONN_ID_LEN` バイト）を導出する。
///
/// quiche のサンプルサーバと同じ方式（秘密鍵つき HMAC なので外部から予測できない）。
/// 呼ばれるのは「未知の DCID を持つ Initial パケット」だけで、確立済み接続の
/// データグラムでは呼ばれない（ハンドシェイク中だけのコスト）。
pub(super) fn derive_server_cid(
    key: &hmac::Key,
    client_dcid: &[u8],
) -> [u8; quiche::MAX_CONN_ID_LEN] {
    let tag = hmac::sign(key, client_dcid);
    let mut cid = [0u8; quiche::MAX_CONN_ID_LEN];
    cid.copy_from_slice(&tag.as_ref()[..quiche::MAX_CONN_ID_LEN]);
    cid
}

#[allow(clippy::too_many_arguments)] // F-151: ダーティ集合/起床キューの受け渡しで増加（ホットパスの単一呼び出し経路）
pub(super) fn process_datagram_segments(
    conns: &mut ConnMapInner,
    data: &mut [u8],
    from: SocketAddr,
    gro_segment_size: Option<u16>,
    cid_key: &hmac::Key,
    quic_config: &Rc<RefCell<quiche::Config>>,
    local_addr: SocketAddr,
    notify: &crate::http3_stream::H3Notify,
    backend_spawner: &crate::http3_stream::BackendSpawner,
    wake_queue: &crate::http3_stream::WakeQueue,
    dirty_queue: &mut VecDeque<ConnKey>,
    now: Instant,
) -> io::Result<()> {
    let total = data.len();
    // GRO セグメントサイズ。None/0（GRO 非適用 = 単発データグラム）の場合は
    // 受信全体を 1 セグメントとして扱う。
    let seg_size = gro_segment_size
        .map(|s| s as usize)
        .filter(|&s| s > 0)
        .unwrap_or(total);

    let mut prev_cid: Option<ConnectionId<'static>> = None;
    let mut offset = 0;
    'segments: while offset < total {
        let start = offset;
        let end = (offset + seg_size).min(total);
        offset = end;

        // パケットヘッダーを解析（同一バッファスライスを後段の conn.recv にも渡す）
        let hdr = match quiche::Header::from_slice(&mut data[start..end], quiche::MAX_CONN_ID_LEN) {
            Ok(v) => v,
            Err(e) => {
                // B-235: 1 通ごとに出るので debug!（偽装した送信元のゴミでログを溢れさせられた）。件数はメトリクスで見る
                debug!("[HTTP/3] Invalid packet header: {}", e);
                crate::metrics::record_http3_invalid_packet();
                // このセグメントのみスキップ。送信処理は呼び出し側ループ後に実行。
                continue;
            }
        };

        // コネクションを検索または作成（直前セグメントと同一 DCID なら判定スキップ）
        let conn_id = match &prev_cid {
            Some(prev) if *prev == hdr.dcid => prev.clone(),
            _ => {
                if !conns.contains_key(&hdr.dcid) {
                    if hdr.ty != quiche::Type::Initial {
                        debug!("[HTTP/3] Non-initial packet for unknown connection");
                        continue;
                    }

                    // B-222: 接続数の上限・ハンドシェイク中の数で受け入れるか・Retry を返すかを決める。
                    // トークン付きの Initial は Retry を経たもの（DCID は Retry で渡したサーバの接続 ID）。
                    if !quiche::version_is_supported(hdr.version) {
                        debug!(
                            "[HTTP/3] Initial with unsupported version {:#x}",
                            hdr.version
                        );
                        continue;
                    }
                    let token = hdr.token.as_deref().unwrap_or(&[]);
                    'conn_id: {
                        let (scid, odcid) = if token.is_empty() {
                            // B-94: サーバ接続 ID はクライアントの元 DCID から決定的に導出する。
                            // クライアントはサーバの最初の応答を受け取るまで Initial を元 DCID 宛てに
                            // 送り続ける（PTO による再送・複数パケットにまたがる ClientHello）。
                            // 乱数で SCID を作っていた頃は、それらが届くたびに別の新規接続を
                            // `accept` していた（孤児接続が増え、クライアントには SCID の異なる
                            // 2 つのサーバ接続から応答が届いてハンドシェイクが崩れる）。
                            let derived = derive_server_cid(cid_key, &hdr.dcid);
                            let derived_ref = ConnectionId::from_ref(&derived);
                            if conns.contains_key(&derived_ref) {
                                let cid = derived_ref.into_owned();
                                prev_cid = Some(cid.clone());
                                break 'conn_id cid;
                            }
                            (derived_ref.into_owned(), None)
                        } else {
                            // Retry 後の Initial の再送は、上の contains_key(&hdr.dcid) に当たる
                            let Some(odcid) = admission::validate_token(&from, token) else {
                                debug!("[HTTP/3] Invalid retry token from {}", from);
                                continue 'segments;
                            };
                            (
                                hdr.dcid.clone().into_owned(),
                                Some(ConnectionId::from_vec(odcid.to_vec())),
                            )
                        };
                        match admission::decide(conns.len(), !token.is_empty()) {
                            admission::Decision::Accept => {}
                            admission::Decision::Retry => {
                                admission::queue_retry(
                                    &hdr.scid,
                                    &hdr.dcid,
                                    hdr.version,
                                    &scid,
                                    from,
                                );
                                continue 'segments;
                            }
                            admission::Decision::Drop => {
                                debug!(
                                    "[HTTP/3] Connection limit reached; dropping Initial from {}",
                                    from
                                );
                                continue 'segments;
                            }
                        }
                        // 新規コネクション
                        let mut config_ref = quic_config.borrow_mut();
                        let conn = match quiche::accept_with_buf_factory::<ZcFactory>(
                            &scid,
                            odcid.as_ref(),
                            local_addr,
                            from,
                            &mut config_ref,
                        ) {
                            Ok(c) => c,
                            Err(e) => {
                                // B-222: 相手の Initial 次第で失敗し得るので、ワーカーごと落とさない
                                debug!("[HTTP/3] accept failed for {}: {}", from, e);
                                continue 'segments;
                            }
                        };
                        drop(config_ref);

                        debug!("[HTTP/3] New connection from {}", from);

                        // F-151（レビュー修正）: 接続 ID の Rc ハンドルは接続の生成時に 1 度だけ
                        // 作る（以降はこの Rc を clone するだけで malloc なしに使い回せる）。
                        let key: ConnKey = Rc::new(scid.clone());
                        // この接続専用の ConnWaker を組み立てる（cid を積んでから notify() する
                        // per-connection 通知）。
                        let waker = crate::http3_stream::ConnWaker::new(
                            key.clone(),
                            wake_queue.clone(),
                            notify.clone(),
                        );
                        let mut handler = Http3Handler::new(
                            conn,
                            from,
                            waker,
                            backend_spawner.clone(),
                            key.clone(),
                        );
                        // B-222: 確立するまでハンドシェイク中として数え、期限を過ぎたら捨てる
                        handler.handshake = Some(admission::HandshakeSlot::new(now));
                        // 新規接続は常にダーティ（後段の recv 直後のダーティ化と二重登録
                        // しないよう、挿入前に直接フラグを立てて自前でキューへ積む）。
                        handler.dirty = true;
                        conns.insert(scid.clone(), Box::new(handler));
                        dirty_queue.push_back(key);

                        prev_cid = Some(scid.clone());
                        scid
                    }
                } else {
                    let cid = hdr.dcid.into_owned();
                    prev_cid = Some(cid.clone());
                    cid
                }
            }
        };

        // パケットを処理（同一スライスをそのまま渡す。追加コピーなし）
        if let Some(handler) = conns.get_mut(&conn_id) {
            let recv_info = quiche::RecvInfo {
                from,
                to: local_addr,
            };

            // 受信の束の時刻（呼び出し側が束ごとに 1 回読む）を「今」として渡す。束のデータグラムはどれも
            // 読む前にカーネルへ届いているので、1 個ずつ読み直すより早くも遅くもならない（v0.8.0）。
            handler.conn.set_time_hint(Some(now));
            match handler.conn.recv(&mut data[start..end], recv_info) {
                Ok(_) => {}
                Err(e) => {
                    // B-235: 相手のパケットごとに起き得るので debug!
                    debug!("[HTTP/3] recv error: {}", e);
                    // エラー時も送信処理は続行
                }
            }
            handler.conn.set_time_hint(None);

            // B-34: クライアントがハンドシェイク直後に HTTP/3 フレームを送るため、
            // 次の recv やメインループ待ちの前に h3 レイヤを確立する（StreamLimit 回避）。
            if handler.conn.is_established() {
                if let Err(e) = handler.init_h3() {
                    warn!("[HTTP/3] eager init_h3 error: {}", e);
                }
            }

            // F-151: recv した接続は必ずダーティ化する（新規接続は既に dirty=true 済みで
            // ここではスキップされる）。`handler` を既に可変借用しているため、`mark_dirty`
            // （`&mut HashMap` を取る版）は使わずインラインでフラグを立てる。
            // `handler.key.clone()`（`Rc::clone`）は参照カウント +1 のみで malloc を伴わない
            // ため、`conn_id`（素の `ConnectionId`）を clone するより安価。
            if !handler.dirty {
                handler.dirty = true;
                dirty_queue.push_back(handler.key.clone());
            }
            // タイマーの再登録はここでは行わない（関数冒頭のコメント参照）。この接続は
            // 上でダーティ化されたので、呼び出し元のダーティ処理ループが
            // `schedule_timer` を呼ぶ。
        }
    }

    Ok(())
}

/// 保留中のパケットを対象コネクションに対して送信する。
///
/// この関数はメインループの各イテレーション末尾で常に 1 回だけ呼び出され、タイムアウト時でも
/// ACKやレスポンスパケットを送信します。
///
/// F-151: 走査対象は**全接続ではなく `targets`（このイテレーションでダーティ処理した接続）
/// だけ**に絞る。sendmmsg / io_uring SENDMSG のバッチ構築ロジック（`finalize_send_entry` /
/// `send_mmsg_flush` / GSO セグメント判定）は一切変更しない。
///
/// F-115 第2段: 従来は per-connection で GSO バッチを組み、**接続ごとに 1 回以上の
/// sendmsg/sendto** を発行していた（-c100 では 1 sweep 最大 ~100 syscall）。本実装は
/// sweep 中に送らずに送信要求を蓄積し（連結バッファ `batch` 内のパケット範囲を指す
/// `sends` インデックス列）、`MMSG_SEND_BATCH` 件到達 or `batch` バイト上限超過時点で
/// `sendmmsg` により **1 syscall = 複数メッセージ** で送出する。既存の flush 条件
/// （宛先変更 / セグメントサイズ不一致 / MAX_GSO_BATCH_BYTES / MAX_GSO_SEGMENTS /
/// 最終ショートセグメント）は「エントリ確定」に読み替える（即送信しない）。同一接続の
/// パケット順序は quiche の send 順（= batch 追記順）のまま保たれる（挙動不変）。
// clippy::await_holding_refcell_ref 許容理由: `connections`（Rc<RefCell<HashMap>>）を
// 借用するのは本 H3 メインループタスクのみ。バックエンドタスクは Rc チャネル + Notify
// 経由で通信し RefCell に触れない（F-32 のアクターモデル）ため、await 中に他タスクが
// 再入借用して panic する経路は存在しない（B-16 とは異なり単一借用者）。
// clippy::too_many_arguments 許容理由: F-151 でダーティ集合の受け渡しが増えた
// （ホットパスの単一呼び出し経路のため構造体化のオーバーヘッドを避ける）。
#[allow(clippy::await_holding_refcell_ref, clippy::too_many_arguments)]
pub(super) async fn send_pending_packets(
    connections: &ConnectionMap,
    socket: &Rc<QuicUdpSocket>,
    _local_addr: SocketAddr,
    mmsg_batch: usize,
    targets: &[ConnKey],
    dirty_queue: &mut VecDeque<ConnKey>,
) {
    let mut conns = connections.borrow_mut();

    // 送信用スクラッチ（send_buf + 連結バッファ + パケット境界 + 送信エントリ + sendmmsg 配列）を
    // スレッドローカルから払い出して再利用する。thread-per-core のためロック不要。
    // take/replace により .await をまたいでスレッドローカルの borrow を保持しないので、
    // 再入（このループ内での多重呼び出し）でも安全。これにより送信のたびに発生していた
    // malloc を排除する。mmsg バッチ幅は `[http3].mmsg_batch_size`（初回確保時に固定）。
    let mut scratch = take_h3_send_scratch(mmsg_batch);
    let H3SendScratch {
        send_buf,
        batch,
        offsets,
        sends,
        mmsg,
    } = &mut scratch;
    batch.clear();
    offsets.clear();
    sends.clear();
    let gso_enabled = socket.gso_enabled();
    let mut closed = Vec::new();

    // sweep 全体で継続する連結バッファのカーソル。「現在構築中のエントリ」は
    // [cur_batch_start, batch.len()) / offsets[cur_offsets_start..] が表す。
    // B-222: 受信の処理で溜めた Retry を先に積む（1 通ずつ単独のエントリ）
    admission::drain_retries(|p| {
        let start = batch.len();
        batch.extend_from_slice(&p.buf[..p.len]);
        sends.push(SendRaw {
            batch_start: start,
            len: p.len,
            seg_size: p.len as u16,
            segments: 1,
            dest: p.to,
        });
    });

    let mut cur_batch_start = batch.len();
    let mut cur_offsets_start = 0usize;
    let mut seg_size = 0usize;
    let mut cur_dest: Option<SocketAddr> = None;

    for cid in targets {
        // `ConnKey`（`Rc<ConnectionId>`）を `&**cid` で deref し、HashMap キー
        // （素の `ConnectionId<'static>`）としてルックアップする。
        let Some(handler) = conns.get_mut(&**cid) else {
            continue; // 既に削除済み（この 1 イテレーション内では通常起きない）。
        };
        // F-60: GSO セグメントサイズの自動調整。quiche の PMTU 探索結果
        // （`max_send_udp_payload_size`: ハンドシェイク中 1200 → 検証後は
        // 設定上限・経路 MTU の小さい方へ成長）に per-connection で追従し、
        // 下限 MIN_UDP_SEND_PAYLOAD / 上限 send_buf 長でクランプする。
        let max_payload = handler
            .conn
            .max_send_udp_payload_size()
            .clamp(MIN_UDP_SEND_PAYLOAD, send_buf.len());

        // F-151: この接続が 1 パケットでも生成したか（送り残しの可能性があるため、
        // 生成があればダーティのまま残す）。
        let mut sent_any = false;

        // この接続のパケットはこの周回の末尾でまとめて送る（sendmmsg / GSO）ので、生成するたびに時刻を
        // 読み直さず 1 回だけ読んで渡す（quiche は send のたびに `Instant::now()` を読んでいた。v0.8.0）。
        handler.conn.set_time_hint(Some(Instant::now()));
        loop {
            // パケットを batch の空き領域へ直接書かせる（以前は send_buf に書いてから batch へコピーしていた。
            // 1 パケットにつき 1 回の memcpy。FreeBSD の h3_file 54KB のプロファイルで memcpy が上位。v0.8.0）
            batch.reserve(max_payload);
            let spare = batch.spare_capacity_mut();
            // SAFETY: quiche の send は out に書くだけで読まない。書いた write バイトだけを下で set_len に反映する
            // （recv_body の spare と同じ扱い。未初期化の部分は読まない）。
            let out = unsafe {
                std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, max_payload)
            };
            let (write, send_info) = match handler.conn.send(out) {
                Ok(v) => {
                    sent_any = true;
                    v
                }
                Err(quiche::Error::Done) => break,
                Err(quiche::Error::CryptoFail) => {
                    // ハンドシェイク途中のため暗号化パケット生成に失敗
                    // 次のイテレーションで再試行される（コネクションは閉じない）
                    debug!("[HTTP/3] CryptoFail (handshake in progress), will retry");
                    break;
                }
                Err(e) => {
                    error!("[HTTP/3] send error: {}", e);
                    handler.conn.close(false, 0x1, b"send error").ok();
                    break;
                }
            };

            // 現在構築中エントリの状態（sweep 全体で継続する batch/offsets の末尾範囲）。
            let cur_seg_count = offsets.len() - cur_offsets_start;
            let cur_bytes = batch.len() - cur_batch_start;

            // 宛先変更 / セグメントサイズ不一致 / GSO バッチ上限（B-18: EMSGSIZE 回避）で
            // 現在エントリを確定する。**即送信せず** sends へ蓄積する（F-115 第2段）。
            let dest_changed = cur_dest.is_some_and(|d| d != send_info.to);
            if dest_changed
                || gso_batch_must_flush_before_append(cur_seg_count, cur_bytes, write, seg_size)
            {
                finalize_send_entry(
                    sends,
                    batch.len(),
                    offsets.as_slice(),
                    gso_enabled,
                    &mut cur_batch_start,
                    &mut cur_offsets_start,
                    &mut seg_size,
                    &mut cur_dest,
                );
            }

            if offsets.len() == cur_offsets_start {
                // 現在エントリの先頭セグメント。
                seg_size = write;
            }
            let start = batch.len();
            // SAFETY: 直前の send が batch の [start, start + write) を書いた（reserve で容量は確保済み）。
            unsafe { batch.set_len(start + write) };
            offsets.push((start, write));
            cur_dest = Some(send_info.to);

            // GSO セグメント上限 or 最終ショートセグメント（< seg_size）→ エントリ確定。
            if (offsets.len() - cur_offsets_start) >= MAX_GSO_SEGMENTS || write < seg_size {
                finalize_send_entry(
                    sends,
                    batch.len(),
                    offsets.as_slice(),
                    gso_enabled,
                    &mut cur_batch_start,
                    &mut cur_offsets_start,
                    &mut seg_size,
                    &mut cur_dest,
                );
                // ここでは現在エントリが空なので、蓄積が閾値を超えたら sendmmsg で送出して
                // batch/offsets/sends をクリアし継続する（RefCell 借用中の await は既存の
                // 単一借用者モデルに準拠）。
                if sends.len() >= mmsg.batch_size() || batch.len() >= H3_SEND_ACCUM_MAX_BYTES {
                    send_mmsg_flush(socket, batch.as_slice(), sends.as_slice(), mmsg).await;
                    batch.clear();
                    offsets.clear();
                    sends.clear();
                    cur_batch_start = 0;
                    cur_offsets_start = 0;
                    // 途中で送った（待った）ので、続きのパケットの「今」を読み直す
                    handler.conn.set_time_hint(Some(Instant::now()));
                }
            }
        }
        handler.conn.set_time_hint(None);

        // 接続末尾: 構築中エントリを確定（送信は閾値到達時 or sweep 末尾）。
        finalize_send_entry(
            sends,
            batch.len(),
            offsets.as_slice(),
            gso_enabled,
            &mut cur_batch_start,
            &mut cur_offsets_start,
            &mut seg_size,
            &mut cur_dest,
        );
        // 接続境界でも蓄積が閾値を超えていれば途中送出する（現在エントリは確定済みで空）。
        // 多接続（各接続が少数パケット）でも蓄積メモリを ~256KB / batch エントリ以内に抑える。
        if sends.len() >= mmsg.batch_size() || batch.len() >= H3_SEND_ACCUM_MAX_BYTES {
            send_mmsg_flush(socket, batch.as_slice(), sends.as_slice(), mmsg).await;
            batch.clear();
            offsets.clear();
            sends.clear();
            cur_batch_start = 0;
            cur_offsets_start = 0;
        }

        if handler.conn.is_closed() {
            debug!("[HTTP/3] Connection closed from {}", handler.peer_addr);
            closed.push(cid.clone());
        } else if sent_any && !handler.dirty {
            // F-151: 送り残しがあるかもしれないため、パケットを生成した接続はダーティの
            // まま残す。`handler` を既に可変借用中のため `mark_dirty` は使わずインラインで
            // フラグを立てる（`conns` の二重借用を避けるため）。
            handler.dirty = true;
            dirty_queue.push_back(cid.clone());
        }
    }

    // sweep 末尾: 残りをまとめて送出（現在エントリは上のループで確定済み）。
    if !sends.is_empty() {
        send_mmsg_flush(socket, batch.as_slice(), sends.as_slice(), mmsg).await;
    }

    for cid in closed {
        conns.remove(&*cid);
    }

    // スクラッチをスレッドローカルへ返却し、次回呼び出しで再利用する（malloc 排除）。
    put_h3_send_scratch(scratch);
}

/// F-115 第2段: 送信エントリ 1 件（`batch` 内の範囲）を表す。`sends` へ蓄積され、
/// sweep 末尾/閾値到達で sendmmsg によりまとめて送出される。ライフタイムを持たないインデックス
/// 表現なので、`H3SendScratch` に載せてスイープ間で再利用できる（per-sweep 確保なし）。
pub(super) struct SendRaw {
    /// `batch` 内の開始オフセット。
    pub(super) batch_start: usize,
    /// 連結長（= GSO バッチ全体 or 単一パケット長）。
    pub(super) len: usize,
    /// GSO セグメントサイズ。
    pub(super) seg_size: u16,
    /// パケット数（1 なら sendmmsg 側で UDP_SEGMENT cmsg を付けない）。
    pub(super) segments: u16,
    /// 送信先。
    pub(super) dest: SocketAddr,
}

/// 構築中のエントリ（`offsets[cur_offsets_start..]` / `batch[cur_batch_start..batch_len]`）を
/// 確定し `sends` へ積む。**送信はしない**（sendmmsg は呼び出し側でまとめて行う）。
///
/// GSO 無効時は multi-segment エントリをパケット境界ごとの単一パケットエントリへ展開する
/// （cmsg 無しの純 sendmmsg。設計判断: F-115 第2段 §2「GSO 無効フォールバック」）。確定後は
/// カーソル（cur_batch_start / cur_offsets_start）を末尾へ進め、seg_size / cur_dest をリセットする。
pub(super) fn finalize_send_entry(
    sends: &mut Vec<SendRaw>,
    batch_len: usize,
    offsets: &[(usize, usize)],
    gso_enabled: bool,
    cur_batch_start: &mut usize,
    cur_offsets_start: &mut usize,
    seg_size: &mut usize,
    cur_dest: &mut Option<SocketAddr>,
) {
    let seg_count = offsets.len() - *cur_offsets_start;
    if seg_count == 0 {
        // 構築中エントリが空。カーソルだけ現在位置へ揃えてリセット。
        *cur_batch_start = batch_len;
        *seg_size = 0;
        *cur_dest = None;
        return;
    }
    // セグメントがある以上 cur_dest は Some。念のため None は安全側でリセットして戻る。
    let Some(dest) = *cur_dest else {
        *cur_batch_start = batch_len;
        *cur_offsets_start = offsets.len();
        *seg_size = 0;
        return;
    };

    if gso_enabled || seg_count == 1 {
        // GSO 有効 or 単一パケット: エントリ 1 件（segments>1 は sendmmsg 側で UDP_SEGMENT cmsg）。
        sends.push(SendRaw {
            batch_start: *cur_batch_start,
            len: batch_len - *cur_batch_start,
            seg_size: *seg_size as u16,
            segments: seg_count as u16,
            dest,
        });
    } else {
        // GSO 無効: パケット境界ごとに 1 エントリへ展開（cmsg なしでも per-packet syscall は削減）。
        for &(off, len) in &offsets[*cur_offsets_start..] {
            sends.push(SendRaw {
                batch_start: off,
                len,
                seg_size: len as u16,
                segments: 1,
                dest,
            });
        }
    }

    *cur_batch_start = batch_len;
    *cur_offsets_start = offsets.len();
    *seg_size = 0;
    *cur_dest = None;
}

/// 蓄積した送信エントリ `sends`（`batch` 内の範囲を指す）を sendmmsg でまとめて送出する。
///
/// `scratch.batch_size()` ごとにチャンク分割し、各チャンクをスタック配列（`MMSG_BATCH_MAX` 上限、
/// ヒープ確保なし）で組み立てて `send_mmsg_async` へ渡す。`sends` は GSO 無効展開により
/// バッチ幅を超え得るため、ここでチャンク化して漏れなく送出する。
pub(super) async fn send_mmsg_flush(
    socket: &Rc<QuicUdpSocket>,
    batch: &[u8],
    sends: &[SendRaw],
    scratch: &mut crate::udp::socket::MmsgSendScratch,
) {
    // F-130 C3: パイプライン化 io_uring SENDMSG が有効なら、libc sendmmsg を使わずに
    // `IORING_OP_SENDMSG` を複数 SQE / 1 submit でまとめて送出する。
    #[cfg(all(target_os = "linux", veil_rt_uring))]
    {
        if let Some(mut send) = take_uring_udp_send() {
            let batch_n = send.capacity();
            let mut i = 0;
            while i < sends.len() {
                let k = (sends.len() - i).min(batch_n);
                // k..MMSG_BATCH_MAX は末尾要素の複製で埋め、`[..k]` で捨てる（複製分は送出されない）。
                let chunk: [crate::udp::socket::SendmmsgEntry; crate::udp::socket::MMSG_BATCH_MAX] =
                    std::array::from_fn(|j| {
                        let s = &sends[i + j.min(k - 1)];
                        crate::udp::socket::SendmmsgEntry {
                            data: &batch[s.batch_start..s.batch_start + s.len],
                            seg_size: s.seg_size,
                            segments: s.segments,
                            dest: s.dest,
                        }
                    });
                if let Err(e) = send.send_batch(&chunk[..k]).await {
                    warn!("[HTTP/3] io_uring sendmsg flush error: {}", e);
                }
                i += k;
            }
            put_uring_udp_send(send);
            return;
        }
    }

    // フォールバック（reactor ビルド / io_uring パイプライン無効時）: 従来の libc sendmmsg。
    let batch_n = scratch.batch_size();
    let mut i = 0;
    while i < sends.len() {
        let k = (sends.len() - i).min(batch_n);
        // k..MMSG_BATCH_MAX は末尾要素の複製で埋め、`[..k]` で捨てる（複製分は送出されない）。
        let chunk: [crate::udp::socket::SendmmsgEntry; crate::udp::socket::MMSG_BATCH_MAX] =
            std::array::from_fn(|j| {
                let s = &sends[i + j.min(k - 1)];
                crate::udp::socket::SendmmsgEntry {
                    data: &batch[s.batch_start..s.batch_start + s.len],
                    seg_size: s.seg_size,
                    segments: s.segments,
                    dest: s.dest,
                }
            });
        if let Err(e) = socket.send_mmsg_async(&chunk[..k], scratch).await {
            warn!("[HTTP/3] sendmmsg flush error: {}", e);
        }
        i += k;
    }
}

/// GSO セグメント上限（UDP GSO の一般的な最大セグメント数）
pub(super) const MAX_GSO_SEGMENTS: usize = 64;

/// F-115: 1 回の readiness あたり非ブロッキングで掻き出す追加データグラム数の**既定**上限。
/// select/タイマー往復を drain バッチ全体で 1 回に償却しつつ、送信・タイムアウト・notify を
/// 過度に遅延させないための上限（Docker veth では 1 データグラム 1 recvmsg のため、この値まで
/// 連続受信すると 1 回の送信スイープへまとめられる）。
///
/// **F-152: この値は `[http3] recv_drain_max` で設定可能になった**（本定数は既定値）。
/// reactor バックエンド（FreeBSD/OpenBSD/NetBSD/macOS・Linux の `--features epoll`）
/// でのみ参照する。Linux 既定の io_uring 経路は `mmsg_batch_size` 本の
/// `IORING_OP_RECVMSG` パイプライン（F-130）なので本値を使わない。
///
/// v0.8.0 で 64 → 1024（FreeBSD aarch64 の `h3_file` 3B、64 接続 × 32 ストリーム）。負荷が高いと
/// 受信キューは常に溜まっていて drain は EAGAIN まで行かずにこの上限で止まる。64 だと 1 イテレーションの
/// データグラムが 32 接続に散り、1 送信あたりの応答が約 1.5 個（nginx は約 3.4 個）になって、
/// 小さいパケットとクライアントの ACK が増えていた。1024 で 205k → 280k req/s（nginx 266k）、
/// p99 は 11 → 14ms。2048 以上はスループットがほぼ横ばいで p99 だけ伸びた。
pub const H3_RECV_DRAIN_MAX_DEFAULT: usize = 1024;

/// F-152: `[http3] recv_drain_max` のクランプ上限。
///
/// 1 イテレーションで掻き出すデータグラムを増やすほど固定費は償却されるが、
/// その間は送信・タイムアウト・バックエンド通知が待たされるため上限を設ける。
/// 受信バッファは `mmsg_batch_size` 個ぶんを使い回すだけなので、本値を大きくしても
/// メモリ使用量は増えない（ループ回数が増えるだけ）。
pub const H3_RECV_DRAIN_MAX_LIMIT: usize = 4096;

/// F-60: 送信セグメントサイズの下限クランプ（RFC 9000 の最小 QUIC データグラム 1200B）
pub(super) const MIN_UDP_SEND_PAYLOAD: usize = 1200;

/// F-60: 送信セグメントサイズの上限クランプ（単一 UDP データグラムの最大ペイロード。
/// 65535 - 8(UDP ヘッダ) - 20(IPv4 ヘッダ) = 65507）。
/// 実際のセグメントサイズは quiche の PMTU 探索と設定 `max_udp_payload_size` の
/// 小さい方に per-connection で自動追従する（`send_pending_packets` 参照）。
pub(super) const MAX_UDP_SEND_PAYLOAD: usize = 65507;

/// B-18: 1 回の sendmsg(UDP_SEGMENT) に載せられる GSO バッチ合計バイト上限。
/// UDP sendmsg のペイロード上限（65507）を超えると EMSGSIZE でバッチ全体が破棄される
/// （QUIC の再送で回復するが帯域・レイテンシを浪費する）ため、超過前に flush する。
/// 従来は MAX_GSO_SEGMENTS(64) × 1350B = 86.4KB まで蓄積し得たため上限超過が起こり得た。
pub(super) const MAX_GSO_BATCH_BYTES: usize = 65507;

/// F-115 第2段: 1 sweep の sendmmsg 蓄積バッファ（`batch`）の途中送出しきい値。
/// エントリ確定（現在エントリが空）のたびにこの値を超えていれば sendmmsg で送出して
/// batch/offsets/sends をクリアし、蓄積バッファの肥大とレイテンシ増を防ぐ。256KB は
/// 多接続時でも 1 回の sendmmsg（最大 16 メッセージ）に見合う実務的な上限（設計書 §2）。
pub(super) const H3_SEND_ACCUM_MAX_BYTES: usize = 256 * 1024;

/// 次パケット（`write` バイト）をバッチへ追加する**前に** flush が必要か判定する。
///
/// - 均一サイズ要求: バッチ内の既存セグメントサイズ `seg_size` と異なるサイズは同居不可
///   （GSO は最終セグメントのみ小さくてよい。大きくなるケースは分割が必要）
/// - B-18: 追加すると合計が `MAX_GSO_BATCH_BYTES` を超える場合は先に flush
#[inline]
pub(super) fn gso_batch_must_flush_before_append(
    offsets_len: usize,
    batch_len: usize,
    write: usize,
    seg_size: usize,
) -> bool {
    if offsets_len == 0 {
        return false;
    }
    write != seg_size || batch_len + write > MAX_GSO_BATCH_BYTES
}

/// `send_pending_packets` 用の送信スクラッチ（スレッドローカルで再利用）。
pub(super) struct H3SendScratch {
    /// quiche の単一パケット書き出し用バッファ（F-60: 上限クランプ長で確保し、
    /// per-connection の動的セグメントサイズでスライスして使用）
    pub(super) send_buf: Vec<u8>,
    /// GSO バッチ連結バッファ（F-115 第2段: sweep 全体で追記し続ける）
    pub(super) batch: Vec<u8>,
    /// バッチ内のパケット境界 (offset, len)（sweep 全体で保持）
    pub(super) offsets: Vec<(usize, usize)>,
    /// F-115 第2段: 確定済み送信エントリ列（`batch` 内範囲のインデックス表現）。
    pub(super) sends: Vec<SendRaw>,
    /// F-115 第2段: sendmmsg 用スクラッチ（Box 固定配列を再利用）。
    pub(super) mmsg: crate::udp::socket::MmsgSendScratch,
}

thread_local! {
    /// 送信スクラッチのスレッドローカル保管庫。thread-per-core のためロック不要。
    static H3_SEND_SCRATCH: std::cell::RefCell<Option<H3SendScratch>> =
        const { std::cell::RefCell::new(None) };
}

// F-130 C3: パイプライン化 io_uring SENDMSG ハンドル（Linux uring バックエンドのみ）。
// `run_http3_server_async` が起動時に 1 回だけ設定し、`send_mmsg_flush` が take/put で
// 払い出す（.await をまたいで RefCell borrow を保持しないため H3_SEND_SCRATCH と同方針）。
#[cfg(all(target_os = "linux", veil_rt_uring))]
thread_local! {
    static URING_UDP_SEND: std::cell::RefCell<Option<crate::runtime::udp_send::UringUdpSend>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(target_os = "linux", veil_rt_uring))]
pub(super) fn take_uring_udp_send() -> Option<crate::runtime::udp_send::UringUdpSend> {
    URING_UDP_SEND.with(|c| c.borrow_mut().take())
}

#[cfg(all(target_os = "linux", veil_rt_uring))]
pub(super) fn put_uring_udp_send(send: crate::runtime::udp_send::UringUdpSend) {
    URING_UDP_SEND.with(|c| *c.borrow_mut() = Some(send));
}

/// スクラッチを払い出す（無ければ新規確保）。take してから返すため、.await をまたいで
/// スレッドローカルの borrow を保持しない（再入安全）。
///
/// `mmsg_batch` は初回確保時のみ効く（既存スクラッチのバッチ幅は維持）。
pub(super) fn take_h3_send_scratch(mmsg_batch: usize) -> H3SendScratch {
    H3_SEND_SCRATCH
        .with(|s| s.borrow_mut().take())
        .unwrap_or_else(|| H3SendScratch {
            send_buf: vec![0u8; MAX_UDP_SEND_PAYLOAD],
            batch: Vec::new(),
            offsets: Vec::new(),
            sends: Vec::new(),
            mmsg: crate::udp::socket::MmsgSendScratch::with_batch(mmsg_batch),
        })
}

/// スクラッチを返却する（次回再利用）。肥大化した batch は一定上限で解放してメモリを抑える。
pub(super) fn put_h3_send_scratch(mut scratch: H3SendScratch) {
    scratch.batch.clear();
    scratch.offsets.clear();
    scratch.sends.clear();
    // バッチが極端に肥大化した場合（>1MB）は確保を手放す。
    if scratch.batch.capacity() > (1 << 20) {
        scratch.batch.shrink_to(64 * 1500);
    }
    H3_SEND_SCRATCH.with(|s| *s.borrow_mut() = Some(scratch));
}

/// HTTP/3 サーバーを起動（同期ラッパー）
///
/// 別スレッドで独自ランタイム（`runtime::block_on`）を作成して実行します。
pub fn run_http3_server(bind_addr: SocketAddr, config: Http3ServerConfig) -> io::Result<()> {
    // カスタム io_uring ランタイムで非同期 HTTP/3 サーバーを実行
    let r =
        crate::runtime::block_on(async move { run_http3_server_async(bind_addr, config).await });
    // B-118: 残った接続タスクをスレッドローカルが生きているうちに破棄する
    crate::runtime::shutdown_thread();
    r
}
