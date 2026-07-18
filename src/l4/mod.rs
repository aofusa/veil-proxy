//! L4 (TCP/UDP) ストリームプロキシモジュール（F-18、UDP は F-124）
//!
//! nginx stream / Envoy TCP・UDP proxy 相当の L4 レイヤープロキシ機能を提供する。
//! HTTP 以外のユースケース（DB 接続、バイナリプロトコル、DNS 等）をサポート。
//!
//! ## 機能
//! - TCP バイダイレクショナル転送（bidirectional stream forwarding）
//! - UDP セッションテーブル方式のデータグラム転送（F-124、`udp` モジュール）
//! - ロードバランシング（ラウンドロビン / 最小接続数）
//! - TLS パススルー（TLS を復号せず upstream に転送、TCP のみ。UDP は DTLS 非対象）
//! - 接続数制限（UDP は同時セッション数上限として扱う）
//! - Prometheus メトリクス統合（metrics feature 有効時）
//! - L4 ヘルスチェック（F-22 と連携。UDP バックエンドも TCP connect ベースのまま）
//!
//! ## 設計制約
//! - データプレーンは独自 io_uring ランタイム（src/runtime/）を使用
//! - ホットパスでのヒープ割り当てを最小化
//! - `l4-proxy` feature が無効の場合はコンパイル対象外

pub mod health;
pub mod proxy;
pub mod server;
pub mod udp;
