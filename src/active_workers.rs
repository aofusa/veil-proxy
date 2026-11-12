//! 接続を負荷に見合う本数のワーカーへ集約する（アクティブセットの自動調整。F-303）
//!
//! 要求の間に眠っては起こされるワーカーは、命令数は同じでもサイクルが倍になる（キャッシュが冷える・
//! 起床の費用）。Docker のコンテナ間（54KB、TLS、100 接続、16 vCPU）の実測で、接続を 16 本のワーカーへ散らすと
//! 1 要求 95µs・44,000 req/s、4 本へ集めると 45µs・81,000 req/s だった。接続はワーカーに固定されるので、
//! 新しい接続を先頭の N 本（アクティブセット）にだけ渡し、N を負荷で動かす。
//!
//! ## 仕組み
//!
//! - **振り分け**: `SO_REUSEPORT` のグループへ「4 タプルのハッシュ % N」の CBPF を付ける。N の変更は
//!   プログラムの差し替えだけ（カーネルが入れ替える。接続の受理は止まらない）。
//!   ワーカーの順とグループの添字を一致させるため、リスナーの `bind` は [`ActiveSet::wait_bind_turn`] で順に行う。
//! - **既存の接続を動かす**: CBPF は新しい接続にしか効かない。ワーカーへ「閉じる接続の割り当て」（[`take_shed`]）を
//!   渡し、HTTP/1.1 の応答の区切りで `Connection: close` を返して閉じる（クライアントは張り直して、新しいプログラムで
//!   振り分けられる）。HTTP/2 の接続は動かさない（GOAWAY を受けて張り直さないクライアントがある。h2load は残りの
//!   要求を失敗にする）。
//! - **N の決め方**（`Policy`）: 起動時・負荷の無い状態が続いた後・接続が急に増えたときは全ワーカー。制御スレッドが
//!   100ms ごとに各ワーカーの使用率（park していない時間の割合）と応答の数を読み、使用率が低ければ（1 回に半分まで）
//!   縮めてみて、スループットが落ちなければ残す。飽和したとき、または飽和はしないが忙しいとき（上流を待つ中継）は
//!   半分ずつ広げてみて、スループットが増えなければ戻す（失敗が続くほど間を空ける）。接続が動かない変更
//!   （HTTP/2 の長い接続だけ）は試すのをやめる。本数の下限はワーカーの数の 4 分の 1（2 以上）。
//!
//! データプレーンの費用は park の前後の時刻の読み取り 2 回と、応答ごとの単独書き手のカウンタ 1 つだけ。
//! 対象は Linux の TCP リスナーで、それ以外の環境ではフック（ワーカーの計測値が無い）は何もしない。

// Linux のみの実装。`SO_ATTACH_REUSEPORT_CBPF` の無い環境（BSD・macOS・Windows）はフックが何もしない
#[cfg(target_os = "linux")]
pub(crate) use imp::{
    note_response, park_begin, park_end, spawn_controller, take_shed, ActiveSet, ConnectionTicket,
};

#[cfg(not(target_os = "linux"))]
pub(crate) use noop::{note_response, park_begin, park_end, take_shed, ConnectionTicket};

/// Linux 以外のフック（何もしない）
#[cfg(not(target_os = "linux"))]
mod noop {
    #[inline]
    pub(crate) fn park_begin() -> u64 {
        0
    }
    #[inline]
    pub(crate) fn park_end(_started: u64) {}
    #[inline]
    pub(crate) fn note_response() {}
    #[inline]
    pub(crate) fn take_shed() -> bool {
        false
    }
    /// 接続の数を数えない
    pub(crate) struct ConnectionTicket;
    impl ConnectionTicket {
        #[inline]
        pub(crate) fn new() -> Self {
            Self
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    // `ActiveSet` が持つ（ロックなし・単独書き手）。`#[repr(align(64))]` はワーカーごとの偽共有を避ける
    /// 1 ワーカーの計測値。
    #[repr(align(64))]
    #[derive(Default)]
    struct WorkerMeter {
        /// park に費やした時間の累計（ナノ秒。ワーカーだけが書く）
        idle_ns: AtomicU64,
        /// park に入った時刻（[`now_ns`]。0 = 起きている）
        parked_since: AtomicU64,
        /// 応答の数の累計（ワーカーだけが書く）
        responses: AtomicU64,
        /// このワーカーの接続の数（ワーカーだけが書く）
        connections: AtomicU32,
        /// 閉じる接続の割り当て（制御スレッドが書き、ワーカーが 1 つずつ取る）
        shed: AtomicU32,
    }

    thread_local! {
        /// このワーカーの計測値（ワーカー以外のスレッドでは null）。
        /// `ActiveSet` はプロセスの終わりまで生きるので、指す先は無効にならない
        static CURRENT: Cell<*const WorkerMeter> = const { Cell::new(std::ptr::null()) };
    }

    #[inline(always)]
    fn current_meter() -> Option<&'static WorkerMeter> {
        // SAFETY: 設定されるポインタは 'static の `ActiveSet` の中を指すか null（`ActiveSet::attach_current_worker`）
        CURRENT.with(|c| unsafe { c.get().as_ref() })
    }

    /// 時刻の基点
    static EPOCH: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);

    /// 基点からのナノ秒（0 は「park 中でない」の印なので 1 を足す）
    #[inline]
    fn now_ns() -> u64 {
        EPOCH.elapsed().as_nanos() as u64 + 1
    }

    /// ワーカーの park（完了の待ち）に入るときに呼ぶ。戻り値は [`park_end`] に渡す
    #[inline]
    pub(crate) fn park_begin() -> u64 {
        match current_meter() {
            Some(m) => {
                let t = now_ns();
                m.parked_since.store(t, Ordering::Relaxed);
                t
            }
            None => 0,
        }
    }

    /// ワーカーの park を抜けたときに呼ぶ（`started` は [`park_begin`] の戻り値）
    #[inline]
    pub(crate) fn park_end(started: u64) {
        if started == 0 {
            return;
        }
        if let Some(m) = current_meter() {
            // 書くのはこのワーカーだけ（制御スレッドは読むだけ）なのでロックの要る命令を使わない
            let idle = m.idle_ns.load(Ordering::Relaxed);
            m.idle_ns
                .store(idle + now_ns().saturating_sub(started), Ordering::Relaxed);
            m.parked_since.store(0, Ordering::Relaxed);
        }
    }

    /// 応答を 1 つ返した（全プロトコルの要求の完了で呼ばれる）
    #[inline]
    pub(crate) fn note_response() {
        if let Some(m) = current_meter() {
            let v = m.responses.load(Ordering::Relaxed);
            m.responses.store(v + 1, Ordering::Relaxed);
        }
    }

    /// この接続を応答の後に閉じる割り当てがあれば 1 つ取る（HTTP/1.1 の要求の入口で呼ぶ）
    #[inline]
    pub(crate) fn take_shed() -> bool {
        let Some(m) = current_meter() else {
            return false;
        };
        let mut cur = m.shed.load(Ordering::Relaxed);
        while cur > 0 {
            match m
                .shed
                .compare_exchange_weak(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(c) => cur = c,
            }
        }
        false
    }

    /// 接続の数を数える（接続のタスクの間生かす。同じワーカーで drop される）
    pub(crate) struct ConnectionTicket;

    impl ConnectionTicket {
        #[inline]
        pub(crate) fn new() -> Self {
            if let Some(m) = current_meter() {
                let v = m.connections.load(Ordering::Relaxed);
                m.connections.store(v + 1, Ordering::Relaxed);
            }
            Self
        }
    }

    impl Drop for ConnectionTicket {
        #[inline]
        fn drop(&mut self) {
            if let Some(m) = current_meter() {
                let v = m.connections.load(Ordering::Relaxed);
                m.connections.store(v.saturating_sub(1), Ordering::Relaxed);
            }
        }
    }

    /// 制御の 1 周の長さ
    const TICK: std::time::Duration = std::time::Duration::from_millis(100);
    /// アクティブセットの最小の本数（これとワーカーの数の `MIN_ACTIVE_DIVISOR` 分の 1 の大きい方）。HTTP/2 の接続は
    /// 後から動かせないので、少ない本数のときに来た接続の束が少数のワーカーに固まりすぎないようにする
    const MIN_ACTIVE: usize = 2;
    const MIN_ACTIVE_DIVISOR: usize = 4;
    /// 飽和とみなす使用率（この周回数続いたら広げてみる）
    const GROW_UTIL: f64 = 0.95;
    const GROW_TICKS: u32 = 2;
    /// これ以上の使用率なら、飽和していなくても広げてみる（中継のように上流を待つ仕事は、ワーカーが足りなくても使用率が
    /// 飽和まで上がらない。広げると速くなるかは測って決める）
    const EXPLORE_UTIL: f64 = 0.5;
    const EXPLORE_TICKS: u32 = 5;
    /// 縮めてみる使用率（この周回数続いたら）
    const SHRINK_UTIL: f64 = 0.80;
    const SHRINK_TICKS: u32 = 5;
    /// 縮めるときに目標にする使用率（集めると 1 要求の費用が下がるので、試して測りながら詰める）
    const SHRINK_TARGET: f64 = 0.85;
    /// これ未満の要求レートでは縮めない（集約して接続を切り替える理由が無い）
    const MIN_RPS: f64 = 1_000.0;
    /// 負荷の無い状態がこの周回数（5 秒）続いたら全ワーカーへ戻す（次に来る接続の重さが分からない。HTTP/2 の接続は
    /// 後から動かせないので、重い HTTP/2 の接続が少数のワーカーへ固まらないように広く受ける）
    const IDLE_RESET_TICKS: u32 = 50;
    /// 変えた後、接続が動く間は測らない周回数
    const SETTLE_TICKS: u32 = 2;
    /// 変えた後の測定の周回数
    const MEASURE_TICKS: u32 = 3;
    /// 広げた結果これだけスループットが増えなければ元に戻す
    const GROW_MIN_GAIN: f64 = 1.05;
    /// 縮めた結果スループットがこれを下回ったら元に戻す（縮め続ける間は最初に縮める前の値と比べる）
    const SHRINK_MAX_LOSS: f64 = 0.97;
    /// 飽和して広げたが速くならず戻した後、次に広げてみるまでの周回数（10 秒。続けて失敗するたびに倍、160 秒まで）
    const GROW_COOLDOWN_TICKS: u64 = 100;
    const GROW_COOLDOWN_MAX_TICKS: u64 = 1_600;
    /// 飽和していないときに広げてみる間隔（30 秒。続けて失敗するたびに倍、300 秒まで）
    const EXPLORE_COOLDOWN_TICKS: u64 = 300;
    const EXPLORE_COOLDOWN_MAX_TICKS: u64 = 3_000;
    /// 縮めて戻した後、次に縮めてみるまでの周回数（30 秒。続けて失敗するたびに倍、300 秒まで）
    const SHRINK_COOLDOWN_TICKS: u64 = 300;
    const SHRINK_COOLDOWN_MAX_TICKS: u64 = 3_000;
    /// 広げて残した後、縮めない周回数（5 秒）
    const AFTER_GROW_SHRINK_HOLD_TICKS: u64 = 50;
    /// 動かせない接続（HTTP/2・WebSocket 等）で試すのをやめておく周回数（30 秒）
    const STUCK_COOLDOWN_TICKS: u64 = 300;
    /// 縮め続ける間の基準（最初に縮める前の要求レート）を使う周回数（30 秒）
    const SHRINK_REF_TICKS: u64 = 300;
    /// 広げた先が公平な割合のこれ未満しか受けていなければ、接続が動いていない
    const MOVED_SHARE_MIN: f64 = 0.25;
    /// 縮めて外したワーカーが前のこれ以上を受けたままなら、接続が動いていない
    const STUCK_SHARE: f64 = 0.5;
    /// 閉じる接続の割り当てを取り消すまでの周回数（残りは閉じない）
    const SHED_EXPIRE_TICKS: u32 = 5;
    /// 接続の数が基準（N を変えたときの数）のこの倍（かつワーカーの数以上の増加）になったら、全ワーカーへ戻して
    /// 探し直す（軽い準備運転の後に本番の接続が来たとき等。少ない本数のまま重い負荷を受けない）
    const SURGE_FACTOR: u32 = 2;
    /// 急増がこの周回数続いたら探し直す（接続を張っては閉じる相手の、瞬間の数の揺れで探し直さない）
    const SURGE_TICKS: u32 = 2;
    /// 接続の数が基準を下回ったままこの周回数（2 秒）続いたら、その間の最大値を新しい基準にする（一時的な減少で
    /// 基準を下げると、戻ってきただけの接続を急増と見誤る）
    const CONNS_FOLLOW_TICKS: u32 = 20;

    /// 既存の接続の動かし方
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Shift {
        /// 振り分けだけ変える（既存の接続はそのまま）
        None,
        /// HTTP/1.1 の接続を応答の区切りで閉じ、張り直しを新しい振り分けで受けて偏りを無くす
        Rebalance,
    }

    /// [`Policy::observe`] の判断
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Action {
        Hold,
        /// N を `to` にする
        Resize {
            to: usize,
            shift: Shift,
        },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ProbeKind {
        /// 飽和したので広げる
        Grow,
        /// 飽和していないが広げてみる（上流を待つ仕事等）
        Explore,
        Shrink,
    }

    /// 変えてみた後の測定（[`Policy::observe`]）
    #[derive(Debug, Clone, Copy)]
    struct Probe {
        kind: ProbeKind,
        /// 変える前の N（戻す先）
        from: usize,
        to: usize,
        /// 変える前の要求レート
        before_rps: f64,
        /// 縮める前に、外すワーカーが受けていた要求レート
        before_moved_rps: f64,
        /// 経過した周回数
        ticks: u32,
        /// 測定区間の要求レートの合計
        after_rps: f64,
        /// 測定区間の、増やした・外したワーカーの要求レートの合計
        after_moved_rps: f64,
    }

    impl Probe {
        /// 増やした・外したワーカーの範囲
        fn moved(&self) -> std::ops::Range<usize> {
            self.from.min(self.to)..self.from.max(self.to)
        }
    }

    /// N の決め方（入力だけで決まる。時間は周回の番号で持つ）
    ///
    /// - 起動時・負荷の無い状態が続いた後・接続が急に増えたときは全ワーカー（接続の重さが分からない間は広く受ける）。
    ///   負荷が変わったら（接続の数の倍増・半減、負荷の無い状態）失敗した試しの待ちを捨てる（前の負荷の結果で次の負荷を
    ///   妨げない）。
    /// - 使用率が低ければ、目標の使用率に見合う本数まで縮めてみて、スループットが落ちなければ残す（集めると
    ///   1 要求の費用が下がるので、縮めた後にさらに縮められることがある。縮め続ける間は最初に縮める前と比べ、少しずつ
    ///   落ちていくのを防ぐ）。外したワーカーの接続が動かなければ（HTTP/2 等）元に戻す。
    /// - 使用率が飽和したら半分ずつ広げてみて、スループットが増えなければ元に戻す（CPU が足りないときは
    ///   ワーカーを増やすと遅くなる）。飽和していなくても使用率が中くらいなら時々広げてみる（上流を待つ仕事は、
    ///   ワーカーが足りなくても使用率が上がりきらない）。増やしたワーカーへ接続が動かなければ広げたままにする。
    #[derive(Debug)]
    struct Policy {
        workers: usize,
        min_active: usize,
        probing: Option<Probe>,
        /// 直近の要求レート（変える前の基準。`recent_len` 個が有効）
        recent_rps: [f64; 3],
        recent_len: usize,
        high_ticks: u32,
        mid_ticks: u32,
        low_ticks: u32,
        idle_ticks: u32,
        /// この周回まで広げない・縮めない
        grow_cooldown_until: u64,
        explore_cooldown_until: u64,
        shrink_cooldown_until: u64,
        /// 次に失敗したときの待ち（失敗が続くたびに倍）
        grow_backoff: u64,
        explore_backoff: u64,
        shrink_backoff: u64,
        /// 縮め続ける間の基準（続けて縮める間に縮める前に見た要求レートの最大と、最初に縮めた周回）
        shrink_ref: Option<(f64, u64)>,
        /// 接続の数の基準（N を変えたときの数。その後に減ったままならその間の最大値まで下げる）
        base_conns: u32,
        /// 基準を下回っている周回数と、その間の最大値
        conns_dip_ticks: u32,
        conns_dip_max: u32,
        /// 接続の急増が続いている周回数
        surge_ticks: u32,
        /// 最後に N を変えた周回
        last_change: u64,
        tick: u64,
    }

    impl Policy {
        fn new(workers: usize) -> Self {
            Self {
                workers,
                min_active: MIN_ACTIVE.max(workers.div_ceil(MIN_ACTIVE_DIVISOR)),
                probing: None,
                recent_rps: [0.0; 3],
                recent_len: 0,
                high_ticks: 0,
                mid_ticks: 0,
                low_ticks: 0,
                idle_ticks: 0,
                grow_cooldown_until: 0,
                explore_cooldown_until: 0,
                shrink_cooldown_until: 0,
                grow_backoff: GROW_COOLDOWN_TICKS,
                explore_backoff: EXPLORE_COOLDOWN_TICKS,
                shrink_backoff: SHRINK_COOLDOWN_TICKS,
                shrink_ref: None,
                base_conns: 0,
                conns_dip_ticks: 0,
                conns_dip_max: 0,
                surge_ticks: 0,
                last_change: 0,
                tick: 0,
            }
        }

        /// 1 周回分の観測から判断する。`n` は今の N、`util` はアクティブセット `[0, n)` の平均使用率、
        /// `worker_rps` はワーカーごとの要求レート（要求/秒）、`conns` はリスナーの接続の数
        fn observe(&mut self, n: usize, util: f64, worker_rps: &[f64], conns: u32) -> Action {
            self.tick += 1;
            let rps: f64 = worker_rps.iter().sum();

            // 接続が急に増えた（負荷の始まり）: 測定中の変更も捨てて探し直す。少ない本数で受けていれば全ワーカーへ戻す
            if conns
                >= (self.base_conns.saturating_mul(SURGE_FACTOR))
                    .max(self.base_conns.saturating_add(self.workers as u32))
            {
                self.surge_ticks += 1;
                if self.surge_ticks >= SURGE_TICKS {
                    self.probing = None;
                    self.restart_search();
                    if n < self.workers {
                        return self.change(self.workers, Shift::Rebalance, conns);
                    }
                    // 全ワーカーで受けている: 戻す先は無いが、前の負荷（準備運転等）で失敗した試しの待ちを捨てる
                    // （待ちが残ると本番の負荷を全ワーカーに散らしたまま受け続ける）
                    self.base_conns = conns;
                    self.surge_ticks = 0;
                    self.conns_dip_ticks = 0;
                    self.conns_dip_max = 0;
                    return Action::Hold;
                }
            } else {
                self.surge_ticks = 0;
            }

            if let Some(p) = self.probing.as_mut() {
                p.ticks += 1;
                if p.ticks <= SETTLE_TICKS {
                    return Action::Hold;
                }
                p.after_rps += rps;
                p.after_moved_rps += worker_rps[p.moved()].iter().sum::<f64>();
                if p.ticks < SETTLE_TICKS + MEASURE_TICKS {
                    return Action::Hold;
                }
                let probe = *p;
                self.probing = None;
                return self.judge(probe, conns);
            }
            // 接続を動かしている間（変えた直後）は基準を下げない（閉じた接続が張り直すまで数が減って見える）
            if self.tick > self.last_change + u64::from(SETTLE_TICKS + SHED_EXPIRE_TICKS) {
                self.follow_conns(conns);
            }

            if rps < MIN_RPS {
                self.high_ticks = 0;
                self.mid_ticks = 0;
                self.low_ticks = 0;
                self.recent_len = 0;
                self.idle_ticks = self.idle_ticks.saturating_add(1);
                if self.idle_ticks >= IDLE_RESET_TICKS {
                    if n < self.workers {
                        self.restart_search();
                        return self.change(self.workers, Shift::None, conns);
                    }
                    // 全ワーカーで受けている: 次の負荷のために失敗の待ちだけ捨てる
                    self.forget_failures();
                }
                return Action::Hold;
            }
            self.idle_ticks = 0;
            self.recent_rps = [self.recent_rps[1], self.recent_rps[2], rps];
            self.recent_len = (self.recent_len + 1).min(self.recent_rps.len());
            let before_rps = self.recent_rps[self.recent_rps.len() - self.recent_len..]
                .iter()
                .sum::<f64>()
                / self.recent_len as f64;

            if util >= GROW_UTIL && n < self.workers {
                self.mid_ticks = 0;
                self.low_ticks = 0;
                self.high_ticks += 1;
                if self.high_ticks >= GROW_TICKS && self.tick >= self.grow_cooldown_until {
                    self.high_ticks = 0;
                    return self.start_probe(ProbeKind::Grow, n, before_rps, 0.0, conns);
                }
                return Action::Hold;
            }
            self.high_ticks = 0;

            if util >= EXPLORE_UTIL && n < self.workers {
                self.mid_ticks += 1;
                if self.mid_ticks >= EXPLORE_TICKS
                    && self.tick >= self.explore_cooldown_until
                    && self.tick >= self.grow_cooldown_until
                {
                    self.mid_ticks = 0;
                    self.low_ticks = 0;
                    return self.start_probe(ProbeKind::Explore, n, before_rps, 0.0, conns);
                }
            } else {
                self.mid_ticks = 0;
            }

            if util <= SHRINK_UTIL && n > self.min_active {
                self.low_ticks += 1;
                if self.low_ticks >= SHRINK_TICKS && self.tick >= self.shrink_cooldown_until {
                    self.low_ticks = 0;
                    self.mid_ticks = 0;
                    // 1 回に外すのは半分まで（使用率は上流を待つ仕事の必要な本数を表さない。半分ずつ測りながら詰める）
                    let target = (n as f64 * util / SHRINK_TARGET).ceil() as usize;
                    let to = target.max(n.div_ceil(2)).clamp(self.min_active, n - 1);
                    let removed: f64 = worker_rps[to..n].iter().sum();
                    // 縮め続ける間の基準（縮める前に見た要求レートの最大。古くなったら取り直す）
                    self.shrink_ref = match self.shrink_ref {
                        Some((best, at)) if self.tick <= at + SHRINK_REF_TICKS => {
                            Some((best.max(before_rps), at))
                        }
                        _ => Some((before_rps, self.tick)),
                    };
                    self.recent_len = 0;
                    self.probing = Some(Probe {
                        kind: ProbeKind::Shrink,
                        from: n,
                        to,
                        before_rps,
                        before_moved_rps: removed,
                        ticks: 0,
                        after_rps: 0.0,
                        after_moved_rps: 0.0,
                    });
                    return self.change(to, Shift::Rebalance, conns);
                }
            } else {
                self.low_ticks = 0;
            }
            Action::Hold
        }

        /// N を変える（変えた周回と接続の数を基準として覚える）
        fn change(&mut self, to: usize, shift: Shift, conns: u32) -> Action {
            self.last_change = self.tick;
            self.base_conns = conns;
            self.conns_dip_ticks = 0;
            self.conns_dip_max = 0;
            self.surge_ticks = 0;
            Action::Resize { to, shift }
        }

        /// 接続の数が基準を下回ったままなら、その間の最大値まで基準を下げる
        fn follow_conns(&mut self, conns: u32) {
            if conns >= self.base_conns {
                self.conns_dip_ticks = 0;
                self.conns_dip_max = 0;
                return;
            }
            self.conns_dip_ticks += 1;
            self.conns_dip_max = self.conns_dip_max.max(conns);
            if self.conns_dip_ticks >= CONNS_FOLLOW_TICKS {
                // 急増と同じ大きさで減った: 負荷が変わったので、前の負荷で失敗した試しの待ちを捨てる
                if self.conns_dip_max.saturating_mul(SURGE_FACTOR) <= self.base_conns
                    && self.base_conns - self.conns_dip_max >= self.workers as u32
                {
                    self.forget_failures();
                }
                self.base_conns = self.conns_dip_max;
                self.conns_dip_ticks = 0;
                self.conns_dip_max = 0;
            }
        }

        /// 起動時と同じ状態から探し直す（全ワーカーへ戻すとき・接続が急に増えたとき）
        fn restart_search(&mut self) {
            self.recent_len = 0;
            self.high_ticks = 0;
            self.mid_ticks = 0;
            self.low_ticks = 0;
            self.idle_ticks = 0;
            self.forget_failures();
        }

        /// 失敗した試しの待ち（続けて失敗するたびに延ばした間隔）と、縮め続ける間の基準を捨てる（負荷が変わったとき）
        fn forget_failures(&mut self) {
            self.grow_cooldown_until = 0;
            self.explore_cooldown_until = 0;
            self.shrink_cooldown_until = 0;
            self.grow_backoff = GROW_COOLDOWN_TICKS;
            self.explore_backoff = EXPLORE_COOLDOWN_TICKS;
            self.shrink_backoff = SHRINK_COOLDOWN_TICKS;
            self.shrink_ref = None;
        }

        /// 半分ずつ広げてみる
        fn start_probe(
            &mut self,
            kind: ProbeKind,
            from: usize,
            before_rps: f64,
            before_moved_rps: f64,
            conns: u32,
        ) -> Action {
            let to = (from + (from / 2).max(1)).min(self.workers);
            // 変えた後の基準は変えた後の観測だけで作り直す
            self.recent_len = 0;
            self.probing = Some(Probe {
                kind,
                from,
                to,
                before_rps,
                before_moved_rps,
                ticks: 0,
                after_rps: 0.0,
                after_moved_rps: 0.0,
            });
            self.change(to, Shift::Rebalance, conns)
        }

        /// 測定が終わった変更を残すか戻すか
        fn judge(&mut self, p: Probe, conns: u32) -> Action {
            let m = f64::from(MEASURE_TICKS);
            let after = p.after_rps / m;
            let moved = p.after_moved_rps / m;
            match p.kind {
                ProbeKind::Grow | ProbeKind::Explore => {
                    let fair = (p.to - p.from) as f64 / p.to as f64;
                    if after <= 0.0 || moved / after < fair * MOVED_SHARE_MIN {
                        // 増やしたワーカーへ接続が動かない（HTTP/2・WebSocket の長い接続だけ）: 広げたままにして
                        // 新しい接続を広く受ける。当分は試さない
                        self.grow_cooldown_until = self.tick + STUCK_COOLDOWN_TICKS;
                        self.explore_cooldown_until = self.tick + STUCK_COOLDOWN_TICKS;
                        self.shrink_cooldown_until = self.tick + STUCK_COOLDOWN_TICKS;
                        return Action::Hold;
                    }
                    if after >= p.before_rps * GROW_MIN_GAIN {
                        // 速くなった: 残す。さらに広げると速くなるかもしれないので続けて試してよい。直後には縮めない
                        self.grow_backoff = GROW_COOLDOWN_TICKS;
                        self.explore_backoff = EXPLORE_COOLDOWN_TICKS;
                        self.explore_cooldown_until = self.tick;
                        self.shrink_cooldown_until = self
                            .shrink_cooldown_until
                            .max(self.tick + AFTER_GROW_SHRINK_HOLD_TICKS);
                        self.shrink_ref = None;
                        return Action::Hold;
                    }
                    // 増やしても速くならない（CPU が足りない・集めた方が速い）: 元に戻す。続けて失敗するほど間を空ける
                    if p.kind == ProbeKind::Grow {
                        self.grow_cooldown_until = self.tick + self.grow_backoff;
                        self.grow_backoff = (self.grow_backoff * 2).min(GROW_COOLDOWN_MAX_TICKS);
                    } else {
                        self.explore_cooldown_until = self.tick + self.explore_backoff;
                        self.explore_backoff =
                            (self.explore_backoff * 2).min(EXPLORE_COOLDOWN_MAX_TICKS);
                    }
                    self.change(p.from, Shift::Rebalance, conns)
                }
                ProbeKind::Shrink => {
                    if p.before_moved_rps > 0.0 && moved >= p.before_moved_rps * STUCK_SHARE {
                        // 外したワーカーの接続が動かない: 振り分けだけ元に戻す（動かす接続は無い）
                        self.shrink_cooldown_until =
                            self.tick + STUCK_COOLDOWN_TICKS.max(self.shrink_backoff);
                        self.shrink_backoff =
                            (self.shrink_backoff * 2).min(SHRINK_COOLDOWN_MAX_TICKS);
                        return self.change(p.from, Shift::None, conns);
                    }
                    // 縮め続ける間は、それまでに縮める前に見た最大とも比べる（1 回ごとの小さな低下が積み重ならない）
                    let reference = match self.shrink_ref {
                        Some((best, at)) if self.tick <= at + SHRINK_REF_TICKS => {
                            best.max(p.before_rps)
                        }
                        _ => p.before_rps,
                    };
                    if after >= reference * SHRINK_MAX_LOSS {
                        // 残す。すぐに広げてみると縮めた分を戻すだけなので、広げてみるのは間を空ける
                        self.shrink_backoff = SHRINK_COOLDOWN_TICKS;
                        self.explore_cooldown_until = self
                            .explore_cooldown_until
                            .max(self.tick + self.explore_backoff);
                        return Action::Hold;
                    }
                    // 遅くなった（集めすぎ）: 元に戻す。続けて失敗するほど間を空ける
                    self.shrink_cooldown_until = self.tick + self.shrink_backoff;
                    self.shrink_backoff = (self.shrink_backoff * 2).min(SHRINK_COOLDOWN_MAX_TICKS);
                    self.change(p.from, Shift::Rebalance, conns)
                }
            }
        }
    }

    // ====================
    // Linux: 振り分けの差し替えと制御スレッド
    // ====================

    pub(crate) use control::{spawn_controller, ActiveSet};

    mod control {
        use super::*;
        use std::sync::atomic::{AtomicI32, AtomicUsize};
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::Instant;

        use ftlog::{debug, info, warn};

        use crate::config::SHUTDOWN_FLAG;
        use crate::runtime::handle::RawFd;

        /// 1 リスナーのアクティブセット
        pub(crate) struct ActiveSet {
            label: Arc<str>,
            meters: Box<[WorkerMeter]>,
            /// 今の N（新しい接続を受けるワーカーの数）
            active: AtomicUsize,
            /// reuseport グループのソケットの複製（-1 = まだ。CBPF の差し替えに使う）
            group_fd: AtomicI32,
            /// `bind` の順番（ワーカーの順とグループの添字を一致させる）
            bind_turn: AtomicUsize,
        }

        static SETS: Mutex<Vec<&'static ActiveSet>> = Mutex::new(Vec::new());

        impl ActiveSet {
            /// `workers` 本のワーカーのリスナーのセットを作る（3 本以上のときだけ。2 本以下は集約の余地が無い）。
            /// 起動時は全ワーカーが受ける
            pub(crate) fn new(label: &Arc<str>, workers: usize) -> Option<&'static Self> {
                if workers < 3 {
                    return None;
                }
                let set: &'static ActiveSet = Box::leak(Box::new(ActiveSet {
                    label: Arc::clone(label),
                    meters: (0..workers).map(|_| WorkerMeter::default()).collect(),
                    active: AtomicUsize::new(workers),
                    group_fd: AtomicI32::new(-1),
                    bind_turn: AtomicUsize::new(0),
                }));
                SETS.lock().unwrap_or_else(|e| e.into_inner()).push(set);
                Some(set)
            }

            /// このワーカーの `bind` の順番が来るまで待つ（起動時に 1 回。同時に `bind` するとグループの添字の順が定まらない）
            pub(crate) fn wait_bind_turn(&self, worker: usize) {
                let start = Instant::now();
                while self.bind_turn.load(Ordering::Acquire) != worker {
                    // 前のワーカーの起動に失敗しても起動を止めない
                    if start.elapsed() > std::time::Duration::from_secs(10) {
                        warn!(
                            "[{}] worker {} gave up waiting for its bind turn",
                            self.label, worker
                        );
                        return;
                    }
                    thread::yield_now();
                }
            }

            /// ワーカーの `bind` が終わった（`fd` はリスナーのソケット。失敗したら `None`）
            pub(crate) fn on_bound(&self, worker: usize, fd: Option<RawFd>) {
                if worker == 0 {
                    if let Some(fd) = fd {
                        self.register_group(fd);
                    }
                }
                self.bind_turn.fetch_add(1, Ordering::Release);
            }

            /// グループの先頭のソケット（最初のワーカーのリスナー）を複製して保持し、全ワーカーへの振り分けを付ける
            fn register_group(&self, fd: RawFd) {
                // ワーカーのソケットの寿命と切り離す（複製は制御スレッドと同じくプロセスの終わりまで持つ）
                // SAFETY: fd は呼び出し側のリスナーの有効な fd。複製を作るだけ
                let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                if dup < 0 {
                    warn!(
                        "[{}] could not duplicate the listening socket: {}; connections are not concentrated",
                        self.label,
                        std::io::Error::last_os_error()
                    );
                    return;
                }
                let n = self.active.load(Ordering::Relaxed);
                match crate::system::attach_reuseport_cbpf(dup, n) {
                    Ok(()) => {
                        self.group_fd.store(dup, Ordering::Release);
                        crate::metrics::set_active_workers(&self.label, n);
                        info!(
                            "[{}] connections are concentrated on as few of the {} workers as the load needs",
                            self.label,
                            self.meters.len()
                        );
                    }
                    Err(e) => {
                        // コンテナの制限等。均等に振り分けるままにする
                        warn!(
                            "[{}] could not attach the connection steering program: {}; connections are not concentrated",
                            self.label, e
                        );
                        // SAFETY: 上で作った複製を閉じるだけ（他に持ち主はいない）
                        unsafe { libc::close(dup) };
                    }
                }
            }

            /// 呼び出したスレッドをワーカー `worker` として計測する
            pub(crate) fn attach_current_worker(&'static self, worker: usize) {
                let meter: *const WorkerMeter = &self.meters[worker];
                CURRENT.with(|c| c.set(meter));
            }

            /// 新しい接続をグループの添字 `base..base + count` へ振り分ける
            fn steer(&self, base: usize, count: usize) -> bool {
                let fd = self.group_fd.load(Ordering::Acquire);
                if fd < 0 {
                    return false;
                }
                match crate::system::attach_reuseport_cbpf_range(fd, base, count) {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(
                            "[{}] could not update the connection steering program: {}",
                            self.label, e
                        );
                        false
                    }
                }
            }

            /// N を `from` から `to` にする。戻り値は「張り直しの間だけ増やしたワーカーへ振り分けている」（決着したら
            /// [`Self::steer`] で `0..to` へ戻す）か。失敗したら `None`
            fn apply(&self, from: usize, to: usize, shift: Shift, conns: &[u32]) -> Option<bool> {
                // 広げて動かすときは、張り直しが落ち着くまで新しい接続を増やしたワーカーへだけ渡す（古いワーカーが
                // 手放した接続が増やした側へ集まって偏りが残らない。全体へ散らすと古い側へ戻る分が出る）
                let staged = shift == Shift::Rebalance && to > from;
                let (base, count) = if staged { (from, to - from) } else { (0, to) };
                if !self.steer(base, count) {
                    return None;
                }
                self.active.store(to, Ordering::Relaxed);
                crate::metrics::set_active_workers(&self.label, to);
                // 新しいプログラムは新しい接続にしか効かないので、既存の接続を動かす割り当てを渡す
                if shift == Shift::Rebalance {
                    if to > from {
                        // 広げる: 先頭の `from` 本から、新しい本数に見合う割合の接続を手放す
                        for (i, m) in self.meters.iter().enumerate().take(from) {
                            let moved = conns[i] as u64 * (to - from) as u64 / to as u64;
                            m.shed.store(moved as u32, Ordering::Relaxed);
                        }
                    } else {
                        // 縮める: 外れたワーカーの接続は全部（応答の区切りで閉じて、残りのワーカーへ戻る）
                        for (i, m) in self.meters.iter().enumerate().take(from).skip(to) {
                            m.shed.store(conns[i], Ordering::Relaxed);
                        }
                    }
                }
                debug!(
                    "[{}] active workers: {} -> {} of {}",
                    self.label,
                    from,
                    to,
                    self.meters.len()
                );
                Some(staged)
            }

            fn expire_sheds(&self) {
                for m in self.meters.iter() {
                    m.shed.store(0, Ordering::Relaxed);
                }
            }
        }

        /// 周回の間に持つ値（ワーカーごとの前回の累計）
        struct ControlState {
            policy: Policy,
            last_idle: Vec<u64>,
            last_responses: Vec<u64>,
            last_tick: Instant,
            /// 接続を手放す割り当てを渡してからの周回数（0 = 渡していない）
            shed_age: u32,
            /// 増やしたワーカーへだけ振り分けている残りの周回数（0 = していない）
            staged_ticks: u32,
            /// 作業用（ワーカーごとの接続の数・要求レート）
            conns: Vec<u32>,
            rps: Vec<f64>,
        }

        /// 全リスナーの制御スレッドを起動する（セットが 1 つも無ければ何もしない）
        pub(crate) fn spawn_controller() {
            let sets: Vec<&'static ActiveSet> =
                SETS.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if sets.is_empty() {
                return;
            }
            // 基点を起動時に作っておく（park の経路で初回の初期化を走らせない）
            once_cell::sync::Lazy::force(&EPOCH);
            let spawned = thread::Builder::new()
                .name("veil-active-workers".to_string())
                .spawn(move || run(sets));
            if let Err(e) = spawned {
                warn!("could not start the connection steering thread: {}", e);
            }
        }

        fn run(sets: Vec<&'static ActiveSet>) {
            let mut states: Vec<ControlState> = sets
                .iter()
                .map(|set| ControlState {
                    policy: Policy::new(set.meters.len()),
                    last_idle: vec![0; set.meters.len()],
                    last_responses: vec![0; set.meters.len()],
                    last_tick: Instant::now(),
                    shed_age: 0,
                    staged_ticks: 0,
                    conns: Vec::with_capacity(set.meters.len()),
                    rps: Vec::with_capacity(set.meters.len()),
                })
                .collect();
            loop {
                // 理由付き allow: データプレーン外の専用スレッド（100ms ごとの周回）。
                #[allow(clippy::disallowed_methods)]
                thread::sleep(TICK);
                if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                    // グループのソケットの複製を閉じる（ワーカーのリスナーが閉じた後も、この複製が残っていると
                    // 終了待ちの間に来た新しい接続が受け取られないまま待たされる）
                    for set in sets.iter() {
                        let fd = set.group_fd.swap(-1, Ordering::AcqRel);
                        if fd >= 0 {
                            // SAFETY: register_group で作った複製。swap で -1 にしたので他から使われない
                            unsafe { libc::close(fd) };
                        }
                    }
                    return;
                }
                for (set, st) in sets.iter().zip(states.iter_mut()) {
                    tick(set, st);
                }
            }
        }

        fn tick(set: &ActiveSet, st: &mut ControlState) {
            let now = Instant::now();
            let elapsed = now.duration_since(st.last_tick).as_nanos() as f64;
            st.last_tick = now;
            if elapsed <= 0.0 || set.group_fd.load(Ordering::Acquire) < 0 {
                return;
            }
            let n = set.active.load(Ordering::Relaxed);
            st.conns.clear();
            st.rps.clear();
            let t = now_ns();
            let mut active_busy = 0.0f64;
            for (i, m) in set.meters.iter().enumerate() {
                let mut idle = m.idle_ns.load(Ordering::Relaxed);
                // いま眠っているワーカーの分は累計にまだ入っていない
                let since = m.parked_since.load(Ordering::Relaxed);
                if since != 0 {
                    idle += t.saturating_sub(since);
                }
                let delta_idle = idle.saturating_sub(st.last_idle[i]);
                st.last_idle[i] = idle;
                let resp = m.responses.load(Ordering::Relaxed);
                st.rps
                    .push(resp.saturating_sub(st.last_responses[i]) as f64 * 1e9 / elapsed);
                st.last_responses[i] = resp;
                st.conns.push(m.connections.load(Ordering::Relaxed));
                if i < n {
                    let idle_frac = (delta_idle as f64 / elapsed).min(1.0);
                    active_busy += 1.0 - idle_frac;
                }
            }
            // 張り直しが落ち着いたら全体（0..n）への振り分けに戻す
            if st.staged_ticks > 0 {
                st.staged_ticks -= 1;
                if st.staged_ticks == 0 {
                    set.steer(0, n);
                }
            }
            // 接続を手放す割り当ての有効期限（期限を過ぎた分は閉じない）
            if st.shed_age > 0 {
                st.shed_age += 1;
                if st.shed_age > SHED_EXPIRE_TICKS {
                    st.shed_age = 0;
                    set.expire_sheds();
                }
            }
            let util = active_busy / n as f64;
            let total_conns: u32 = st.conns.iter().sum();
            let action = st.policy.observe(n, util, &st.rps, total_conns);
            if let Action::Resize { to, shift } = action {
                if to != n {
                    if let Some(staged) = set.apply(n, to, shift, &st.conns) {
                        if shift == Shift::Rebalance {
                            st.shed_age = 1;
                        }
                        st.staged_ticks = if staged { SETTLE_TICKS } else { 0 };
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 試験の接続の数（N を変えたときの数と同じにして、急増と見なさない）
        const CONNS: u32 = 100;

        /// 16 本のワーカーのうち先頭 `n` 本が均等に `total` 要求/秒を受けている観測
        fn spread(n: usize, total: f64) -> Vec<f64> {
            (0..16)
                .map(|i| if i < n { total / n as f64 } else { 0.0 })
                .collect()
        }

        /// 16 本のワーカーの制御を、接続の数の基準を `conns` にして作る（N を変えた直後と同じ）
        fn policy(conns: u32) -> Policy {
            let mut p = Policy::new(16);
            p.base_conns = conns;
            p
        }

        fn resize(to: usize) -> Action {
            Action::Resize {
                to,
                shift: Shift::Rebalance,
            }
        }

        /// 判断が出るまで同じ観測を与える
        fn drive(
            p: &mut Policy,
            n: usize,
            util: f64,
            rps: &[f64],
            conns: u32,
            limit: u64,
        ) -> Action {
            for _ in 0..limit {
                let a = p.observe(n, util, rps, conns);
                if a != Action::Hold {
                    return a;
                }
            }
            Action::Hold
        }

        /// 変更の後の落ち着き・測定の周回を与えて判定を得る
        fn settle_and_measure(p: &mut Policy, n: usize, util: f64, rps: &[f64]) -> Action {
            let mut out = Action::Hold;
            for _ in 0..(SETTLE_TICKS + MEASURE_TICKS) {
                out = p.observe(n, util, rps, CONNS);
            }
            assert!(p.probing.is_none());
            out
        }

        #[test]
        fn starts_with_all_workers_and_shrinks_by_at_most_half_while_throughput_holds() {
            let mut p = Policy::new(16);
            // 16 本で使用率 0.26（コンテナ間の 54KB・100 接続の実測に近い）。使用率から見た本数は 5 だが半分まで
            let a = drive(&mut p, 16, 0.26, &spread(16, 44_000.0), CONNS, 20);
            assert_eq!(a, resize(8));
            // 集めると速くなった: 残す
            assert_eq!(
                settle_and_measure(&mut p, 8, 0.50, &spread(8, 60_000.0)),
                Action::Hold
            );
            let a = drive(&mut p, 8, 0.50, &spread(8, 60_000.0), CONNS, 20);
            assert_eq!(a, resize(5));
            assert_eq!(
                settle_and_measure(&mut p, 5, 0.77, &spread(5, 70_000.0)),
                Action::Hold
            );
            // 最小の本数（16 本の 4 分の 1）まで
            let a = drive(&mut p, 5, 0.77, &spread(5, 70_000.0), CONNS, 20);
            assert_eq!(a, resize(4));
            assert_eq!(
                settle_and_measure(&mut p, 4, 0.90, &spread(4, 82_000.0)),
                Action::Hold
            );
            // 使用率 0.90 は縮めない。縮めて残した直後は広げてもみない
            assert_eq!(
                drive(&mut p, 4, 0.90, &spread(4, 82_000.0), CONNS, 100),
                Action::Hold
            );
        }

        #[test]
        fn the_minimum_is_a_quarter_of_the_workers() {
            assert_eq!(Policy::new(1).min_active, MIN_ACTIVE);
            assert_eq!(Policy::new(4).min_active, MIN_ACTIVE);
            assert_eq!(Policy::new(16).min_active, 4);
            assert_eq!(Policy::new(64).min_active, 16);
        }

        #[test]
        fn reverts_a_shrink_that_loses_throughput_and_backs_off() {
            let mut p = policy(CONNS);
            let a = drive(&mut p, 8, 0.45, &spread(8, 80_000.0), CONNS, 20);
            assert_eq!(a, resize(5));
            assert_eq!(
                settle_and_measure(&mut p, 5, 1.0, &spread(5, 66_000.0)),
                resize(8)
            );
            // 当分は縮めない
            let quiet = SHRINK_COOLDOWN_TICKS - 1;
            assert_eq!(
                drive(&mut p, 8, 0.45, &spread(8, 80_000.0), CONNS, quiet),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 8, 0.45, &spread(8, 80_000.0), CONNS, 1),
                resize(5)
            );
            // 続けて失敗したら倍の間を空ける
            assert_eq!(
                settle_and_measure(&mut p, 5, 1.0, &spread(5, 66_000.0)),
                resize(8)
            );
            let quiet = SHRINK_COOLDOWN_TICKS * 2 - 1;
            assert_eq!(
                drive(&mut p, 8, 0.45, &spread(8, 80_000.0), CONNS, quiet),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 8, 0.45, &spread(8, 80_000.0), CONNS, 1),
                resize(5)
            );
        }

        #[test]
        fn a_shrink_chain_is_compared_with_the_throughput_before_the_first_shrink() {
            let mut p = Policy::new(16);
            assert_eq!(
                drive(&mut p, 16, 0.30, &spread(16, 100_000.0), CONNS, 20),
                resize(8)
            );
            assert_eq!(
                settle_and_measure(&mut p, 8, 0.50, &spread(8, 98_000.0)),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 8, 0.50, &spread(8, 98_000.0), CONNS, 20),
                resize(5)
            );
            // 直前（98k）から見れば 2% の低下だが、最初に縮める前（100k）から 4% 落ちている: 戻す
            assert_eq!(
                settle_and_measure(&mut p, 5, 0.80, &spread(5, 96_000.0)),
                resize(8)
            );
        }

        #[test]
        fn a_shrink_chain_does_not_ratchet_down_after_a_gain() {
            // 中継: 16 本から 8 本で速くなった後、1 本ずつ縮めるたびに 2% ずつ落ちる
            let mut p = Policy::new(16);
            assert_eq!(
                drive(&mut p, 16, 0.25, &spread(16, 38_000.0), CONNS, 20),
                resize(8)
            );
            assert_eq!(
                settle_and_measure(&mut p, 8, 0.75, &spread(8, 45_000.0)),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 8, 0.75, &spread(8, 45_000.0), CONNS, 20),
                resize(8 - 1)
            );
            assert_eq!(
                settle_and_measure(&mut p, 7, 0.78, &spread(7, 44_000.0)),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 7, 0.78, &spread(7, 44_000.0), CONNS, 20),
                resize(7 - 1)
            );
            // 直前（44k）からは 2% だが、8 本の 45k からは 4.4% 落ちている: 戻す
            assert_eq!(
                settle_and_measure(&mut p, 6, 0.80, &spread(6, 43_000.0)),
                resize(7)
            );
        }

        #[test]
        fn a_shrink_that_cannot_move_connections_is_undone_without_shedding() {
            let mut p = Policy::new(16);
            let a = drive(&mut p, 16, 0.30, &spread(16, 50_000.0), CONNS, 20);
            let Action::Resize { to, .. } = a else {
                panic!("did not shrink")
            };
            // 外したワーカーが同じだけ受け続けている（HTTP/2 の長い接続）
            assert_eq!(
                settle_and_measure(&mut p, to, 0.30, &spread(16, 50_000.0)),
                Action::Resize {
                    to: 16,
                    shift: Shift::None
                }
            );
            assert!(p.shrink_cooldown_until >= p.tick + STUCK_COOLDOWN_TICKS);
        }

        #[test]
        fn grows_by_half_when_saturated_and_keeps_a_gain() {
            let mut p = policy(CONNS);
            let a = drive(&mut p, 4, 0.99, &spread(4, 20_000.0), CONNS, 10);
            assert_eq!(a, resize(6));
            assert_eq!(
                settle_and_measure(&mut p, 6, 0.99, &spread(6, 28_000.0)),
                Action::Hold
            );
            let a = drive(&mut p, 6, 0.99, &spread(6, 28_000.0), CONNS, 10);
            assert_eq!(a, resize(9));
        }

        #[test]
        fn reverts_a_growth_without_a_gain_and_backs_off() {
            let mut p = policy(CONNS);
            drive(&mut p, 4, 0.99, &spread(4, 80_000.0), CONNS, 10);
            assert_eq!(
                settle_and_measure(&mut p, 6, 0.70, &spread(6, 63_000.0)),
                resize(4)
            );
            // 当分は飽和しても広げない
            let quiet = GROW_COOLDOWN_TICKS - 1;
            assert_eq!(
                drive(&mut p, 4, 0.99, &spread(4, 80_000.0), CONNS, quiet),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 4, 0.99, &spread(4, 80_000.0), CONNS, 1),
                resize(6)
            );
            // 続けて失敗したら倍の間を空ける
            assert_eq!(
                settle_and_measure(&mut p, 6, 0.70, &spread(6, 63_000.0)),
                resize(4)
            );
            let quiet = GROW_COOLDOWN_TICKS * 2 - 1;
            assert_eq!(
                drive(&mut p, 4, 0.99, &spread(4, 80_000.0), CONNS, quiet),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 4, 0.99, &spread(4, 80_000.0), CONNS, 1),
                resize(6)
            );
        }

        #[test]
        fn keeps_a_growth_that_could_not_move_connections() {
            let mut p = policy(CONNS);
            drive(&mut p, 4, 0.99, &spread(4, 30_000.0), CONNS, 10);
            // 増やしたワーカーが何も受けていない（HTTP/2 の長い接続だけ）: 戻さない
            assert_eq!(
                settle_and_measure(&mut p, 6, 0.70, &spread(4, 30_000.0)),
                Action::Hold
            );
        }

        #[test]
        fn explores_a_wider_set_when_busy_but_not_saturated() {
            // 上流を待つ中継: 使用率は飽和しないが、広げると速くなる
            let mut p = policy(CONNS);
            let a = drive(&mut p, 8, 0.85, &spread(8, 40_000.0), CONNS, 20);
            assert_eq!(a, resize(12));
            assert_eq!(
                settle_and_measure(&mut p, 12, 0.85, &spread(12, 46_000.0)),
                Action::Hold
            );
            // 速くなったので続けて試す
            let a = drive(&mut p, 12, 0.85, &spread(12, 46_000.0), CONNS, 20);
            assert_eq!(a, resize(16));
        }

        #[test]
        fn explore_backs_off_after_failures() {
            let mut p = policy(CONNS);
            assert_eq!(
                drive(&mut p, 8, 0.85, &spread(8, 40_000.0), CONNS, 20),
                resize(12)
            );
            assert_eq!(
                settle_and_measure(&mut p, 12, 0.60, &spread(12, 40_000.0)),
                resize(8)
            );
            let quiet = EXPLORE_COOLDOWN_TICKS - 1;
            assert_eq!(
                drive(&mut p, 8, 0.85, &spread(8, 40_000.0), CONNS, quiet),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 8, 0.85, &spread(8, 40_000.0), CONNS, 1),
                resize(12)
            );
            assert_eq!(
                settle_and_measure(&mut p, 12, 0.60, &spread(12, 40_000.0)),
                resize(8)
            );
            let quiet = EXPLORE_COOLDOWN_TICKS * 2 - 1;
            assert_eq!(
                drive(&mut p, 8, 0.85, &spread(8, 40_000.0), CONNS, quiet),
                Action::Hold
            );
            assert_eq!(
                drive(&mut p, 8, 0.85, &spread(8, 40_000.0), CONNS, 1),
                resize(12)
            );
        }

        #[test]
        fn a_surge_of_connections_resets_to_all_workers() {
            // 軽い準備運転（10 接続）で縮めた後に、本番の 100 接続が来る
            let mut p = Policy::new(16);
            assert_eq!(
                drive(&mut p, 16, 0.12, &spread(16, 13_000.0), 10, 20),
                resize(8)
            );
            for _ in 0..(SETTLE_TICKS + MEASURE_TICKS) {
                p.observe(8, 0.24, &spread(8, 13_000.0), 10);
            }
            assert!(p.probing.is_none());
            // 1 周回だけの急増では探し直さない
            assert_eq!(p.observe(8, 0.24, &spread(8, 13_000.0), 100), Action::Hold);
            assert_eq!(p.observe(8, 0.70, &spread(8, 22_000.0), 100), resize(16));
            // 全ワーカーから探し直す
            assert_eq!(
                drive(&mut p, 16, 0.25, &spread(16, 38_000.0), 100, 20),
                resize(8)
            );
        }

        /// 縮めてみたが遅くなったので戻した（縮めるのを当分試さない）状態にする
        fn fail_a_shrink(p: &mut Policy, conns: u32) {
            assert_eq!(
                drive(p, 16, 0.15, &spread(16, 24_000.0), conns, 20),
                resize(8)
            );
            let mut out = Action::Hold;
            for _ in 0..(SETTLE_TICKS + MEASURE_TICKS) {
                out = p.observe(8, 0.27, &spread(8, 20_000.0), conns);
            }
            assert_eq!(out, resize(16));
            assert!(p.shrink_cooldown_until > p.tick + 100);
        }

        #[test]
        fn a_surge_at_full_width_forgets_failed_probes() {
            // 準備運転（10 接続）で縮めてみて失敗した直後に本番の 100 接続が来る（以前は失敗の待ちが残り、本番の
            // 間ずっと全ワーカーに散らしたままだった）
            let mut p = Policy::new(16);
            fail_a_shrink(&mut p, 10);
            assert_eq!(
                p.observe(16, 0.23, &spread(16, 45_000.0), 100),
                Action::Hold
            );
            assert_eq!(
                p.observe(16, 0.23, &spread(16, 45_000.0), 100),
                Action::Hold
            );
            assert_eq!(p.shrink_cooldown_until, 0);
            assert_eq!(p.base_conns, 100);
            assert_eq!(
                drive(&mut p, 16, 0.23, &spread(16, 45_000.0), 100, 10),
                resize(8)
            );
        }

        #[test]
        fn a_large_drop_or_a_quiet_period_forgets_failed_probes() {
            // 接続が半分以下（かつワーカーの数以上の減少）に減ったまま続いた
            let mut p = policy(100);
            fail_a_shrink(&mut p, 100);
            assert_eq!(
                drive(&mut p, 16, 0.15, &spread(16, 12_000.0), 40, 40),
                resize(8)
            );
            assert_eq!(p.base_conns, 40);
            // 少し減っただけなら待ちは残る
            let mut p = policy(100);
            fail_a_shrink(&mut p, 100);
            assert_eq!(
                drive(&mut p, 16, 0.15, &spread(16, 12_000.0), 70, 40),
                Action::Hold
            );
            // 負荷の無い状態が続いた（接続は開いたまま）
            let mut p = policy(100);
            fail_a_shrink(&mut p, 100);
            for _ in 0..IDLE_RESET_TICKS {
                assert_eq!(p.observe(16, 0.0, &spread(16, 0.0), 100), Action::Hold);
            }
            assert_eq!(
                drive(&mut p, 16, 0.15, &spread(16, 24_000.0), 100, 10),
                resize(8)
            );
        }

        #[test]
        fn a_short_dip_in_connections_keeps_the_surge_baseline() {
            let observe_n = |p: &mut Policy, conns: u32, ticks: u32| {
                let mut out = Action::Hold;
                for _ in 0..ticks {
                    out = p.observe(8, 0.90, &spread(8, 40_000.0), conns);
                    if out != Action::Hold {
                        break;
                    }
                }
                out
            };
            // 一時的に減って戻っただけ: 急増ではない
            let mut p = policy(30);
            p.explore_cooldown_until = u64::MAX;
            assert_eq!(observe_n(&mut p, 30, 10), Action::Hold);
            assert_eq!(observe_n(&mut p, 20, CONNS_FOLLOW_TICKS - 1), Action::Hold);
            assert_eq!(observe_n(&mut p, 40, 10), Action::Hold);
            // 減ったままなら基準を下げる: その後の増加は急増（減り方は小さいので失敗の待ちは捨てない）
            let mut p = policy(30);
            p.explore_cooldown_until = u64::MAX;
            assert_eq!(observe_n(&mut p, 30, 10), Action::Hold);
            assert_eq!(observe_n(&mut p, 20, CONNS_FOLLOW_TICKS), Action::Hold);
            assert_eq!(p.base_conns, 20);
            assert_eq!(observe_n(&mut p, 40, 10), resize(16));
        }

        #[test]
        fn resets_to_all_workers_after_a_quiet_period() {
            let mut p = policy(CONNS);
            let quiet = spread(4, 10.0);
            for _ in 0..(IDLE_RESET_TICKS - 1) {
                assert_eq!(p.observe(4, 0.01, &quiet, CONNS), Action::Hold);
            }
            assert_eq!(
                p.observe(4, 0.01, &quiet, CONNS),
                Action::Resize {
                    to: 16,
                    shift: Shift::None
                }
            );
        }

        #[test]
        fn does_not_shrink_below_the_minimum_or_on_low_rps() {
            let mut p = policy(CONNS);
            let min = p.min_active;
            assert_eq!(
                drive(&mut p, min, 0.05, &spread(min, 30_000.0), CONNS, 100),
                Action::Hold
            );
            let mut p = policy(CONNS);
            assert_eq!(
                drive(&mut p, 8, 0.05, &spread(8, 500.0), CONNS, 40),
                Action::Hold
            );
        }

        #[test]
        fn take_shed_without_a_worker_meter_is_false() {
            assert!(!take_shed());
        }
    }
}
