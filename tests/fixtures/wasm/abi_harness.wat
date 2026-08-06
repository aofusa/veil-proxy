;; F-134: Proxy-Wasm ABI v0.2.1 適合度テスト用の手書きハーネスモジュール。
;;
;; 実際の proxy-wasm SDK が生成する `.wasm`（tests/fixtures/wasm/*.wasm）は
;; ゲスト側のロジック（proxy_on_request_headers 等のコールバック）に閉じ込められており、
;; ホスト関数を任意の生引数（範囲外オフセット・不正な enum 値等）で直接叩けない。
;;
;; このハーネスは Proxy-Wasm ホスト関数を「env」からそのままインポートし、
;; 各関数に対して 1:1 で転送するだけの `test_*` エクスポートを提供する。
;; 統合テスト（tests/proxy_wasm_conformance.rs）はこの `test_*` 関数を
;; `Instance::get_typed_func` 経由で直接呼び出し、渡した引数と返ってきた
;; Proxy-Wasm ステータスコードを検証する。
;;
;; メモリレイアウト:
;;   [0, 131072)    テストコードが自由に読み書きするスクラッチ領域
;;   [131072, ...)  proxy_on_memory_allocate のバンプアロケータ領域
;;                  （proxy_get_buffer_bytes 等、ホストが可変長データを
;;                  書き戻す際にここから確保する）
(module
  (import "env" "proxy_log" (func $proxy_log (param i32 i32 i32) (result i32)))
  (import "env" "proxy_get_current_time_nanoseconds" (func $proxy_get_current_time_nanoseconds (param i32) (result i32)))
  (import "env" "proxy_set_tick_period_milliseconds" (func $proxy_set_tick_period_milliseconds (param i32) (result i32)))
  (import "env" "proxy_get_buffer_bytes" (func $proxy_get_buffer_bytes (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_set_buffer_bytes" (func $proxy_set_buffer_bytes (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_get_header_map_pairs" (func $proxy_get_header_map_pairs (param i32 i32 i32) (result i32)))
  (import "env" "proxy_set_header_map_pairs" (func $proxy_set_header_map_pairs (param i32 i32 i32) (result i32)))
  (import "env" "proxy_get_header_map_value" (func $proxy_get_header_map_value (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_replace_header_map_value" (func $proxy_replace_header_map_value (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_add_header_map_value" (func $proxy_add_header_map_value (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_remove_header_map_value" (func $proxy_remove_header_map_value (param i32 i32 i32) (result i32)))
  (import "env" "proxy_get_property" (func $proxy_get_property (param i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_set_property" (func $proxy_set_property (param i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_get_shared_data" (func $proxy_get_shared_data (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_set_shared_data" (func $proxy_set_shared_data (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_register_shared_queue" (func $proxy_register_shared_queue (param i32 i32 i32) (result i32)))
  (import "env" "proxy_resolve_shared_queue" (func $proxy_resolve_shared_queue (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_dequeue_shared_queue" (func $proxy_dequeue_shared_queue (param i32 i32 i32) (result i32)))
  (import "env" "proxy_enqueue_shared_queue" (func $proxy_enqueue_shared_queue (param i32 i32 i32) (result i32)))
  (import "env" "proxy_http_call" (func $proxy_http_call (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_grpc_call" (func $proxy_grpc_call (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_grpc_stream" (func $proxy_grpc_stream (param i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_grpc_send" (func $proxy_grpc_send (param i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_grpc_cancel" (func $proxy_grpc_cancel (param i32) (result i32)))
  (import "env" "proxy_grpc_close" (func $proxy_grpc_close (param i32) (result i32)))
  (import "env" "proxy_send_local_response" (func $proxy_send_local_response (param i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_continue_stream" (func $proxy_continue_stream (param i32) (result i32)))
  (import "env" "proxy_close_stream" (func $proxy_close_stream (param i32) (result i32)))
  (import "env" "proxy_set_effective_context" (func $proxy_set_effective_context (param i32) (result i32)))
  (import "env" "proxy_done" (func $proxy_done (result i32)))
  (import "env" "proxy_get_status" (func $proxy_get_status (param i32 i32 i32) (result i32)))
  (import "env" "proxy_define_metric" (func $proxy_define_metric (param i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_increment_metric" (func $proxy_increment_metric (param i32 i64) (result i32)))
  (import "env" "proxy_record_metric" (func $proxy_record_metric (param i32 i64) (result i32)))
  (import "env" "proxy_get_metric" (func $proxy_get_metric (param i32 i32) (result i32)))
  (import "env" "proxy_call_foreign_function" (func $proxy_call_foreign_function (param i32 i32 i32 i32 i32 i32) (result i32)))

  (memory (export "memory") 16 64)

  ;; バンプアロケータ: 先頭 128KiB はテストのスクラッチ領域として予約する。
  (global $bump (mut i32) (i32.const 131072))

  (func (export "proxy_on_memory_allocate") (param $size i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
    (local.get $p)
  )

  (func (export "test_proxy_log") (param i32 i32 i32) (result i32)
    (call $proxy_log (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_get_current_time_nanoseconds") (param i32) (result i32)
    (call $proxy_get_current_time_nanoseconds (local.get 0)))

  (func (export "test_proxy_set_tick_period_milliseconds") (param i32) (result i32)
    (call $proxy_set_tick_period_milliseconds (local.get 0)))

  (func (export "test_proxy_get_buffer_bytes") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_get_buffer_bytes (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_set_buffer_bytes") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_set_buffer_bytes (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_get_header_map_pairs") (param i32 i32 i32) (result i32)
    (call $proxy_get_header_map_pairs (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_set_header_map_pairs") (param i32 i32 i32) (result i32)
    (call $proxy_set_header_map_pairs (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_get_header_map_value") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_get_header_map_value (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_replace_header_map_value") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_replace_header_map_value (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_add_header_map_value") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_add_header_map_value (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_remove_header_map_value") (param i32 i32 i32) (result i32)
    (call $proxy_remove_header_map_value (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_get_property") (param i32 i32 i32 i32) (result i32)
    (call $proxy_get_property (local.get 0) (local.get 1) (local.get 2) (local.get 3)))

  (func (export "test_proxy_set_property") (param i32 i32 i32 i32) (result i32)
    (call $proxy_set_property (local.get 0) (local.get 1) (local.get 2) (local.get 3)))

  (func (export "test_proxy_get_shared_data") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_get_shared_data (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_set_shared_data") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_set_shared_data (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_register_shared_queue") (param i32 i32 i32) (result i32)
    (call $proxy_register_shared_queue (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_resolve_shared_queue") (param i32 i32 i32 i32 i32) (result i32)
    (call $proxy_resolve_shared_queue (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)))

  (func (export "test_proxy_dequeue_shared_queue") (param i32 i32 i32) (result i32)
    (call $proxy_dequeue_shared_queue (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_enqueue_shared_queue") (param i32 i32 i32) (result i32)
    (call $proxy_enqueue_shared_queue (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_http_call") (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
    (call $proxy_http_call
      (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4)
      (local.get 5) (local.get 6) (local.get 7) (local.get 8) (local.get 9)))

  (func (export "test_proxy_grpc_call") (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
    (call $proxy_grpc_call
      (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4) (local.get 5)
      (local.get 6) (local.get 7) (local.get 8) (local.get 9) (local.get 10) (local.get 11)))

  (func (export "test_proxy_grpc_stream") (param i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
    (call $proxy_grpc_stream
      (local.get 0) (local.get 1) (local.get 2) (local.get 3)
      (local.get 4) (local.get 5) (local.get 6) (local.get 7) (local.get 8)))

  (func (export "test_proxy_grpc_send") (param i32 i32 i32 i32) (result i32)
    (call $proxy_grpc_send (local.get 0) (local.get 1) (local.get 2) (local.get 3)))

  (func (export "test_proxy_grpc_cancel") (param i32) (result i32)
    (call $proxy_grpc_cancel (local.get 0)))

  (func (export "test_proxy_grpc_close") (param i32) (result i32)
    (call $proxy_grpc_close (local.get 0)))

  (func (export "test_proxy_send_local_response") (param i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
    (call $proxy_send_local_response
      (local.get 0) (local.get 1) (local.get 2) (local.get 3)
      (local.get 4) (local.get 5) (local.get 6) (local.get 7)))

  (func (export "test_proxy_continue_stream") (param i32) (result i32)
    (call $proxy_continue_stream (local.get 0)))

  (func (export "test_proxy_close_stream") (param i32) (result i32)
    (call $proxy_close_stream (local.get 0)))

  (func (export "test_proxy_set_effective_context") (param i32) (result i32)
    (call $proxy_set_effective_context (local.get 0)))

  (func (export "test_proxy_done") (result i32)
    (call $proxy_done))

  (func (export "test_proxy_get_status") (param i32 i32 i32) (result i32)
    (call $proxy_get_status (local.get 0) (local.get 1) (local.get 2)))

  (func (export "test_proxy_define_metric") (param i32 i32 i32 i32) (result i32)
    (call $proxy_define_metric (local.get 0) (local.get 1) (local.get 2) (local.get 3)))

  (func (export "test_proxy_increment_metric") (param i32 i64) (result i32)
    (call $proxy_increment_metric (local.get 0) (local.get 1)))

  (func (export "test_proxy_record_metric") (param i32 i64) (result i32)
    (call $proxy_record_metric (local.get 0) (local.get 1)))

  (func (export "test_proxy_get_metric") (param i32 i32) (result i32)
    (call $proxy_get_metric (local.get 0) (local.get 1)))

  (func (export "test_proxy_call_foreign_function") (param i32 i32 i32 i32 i32 i32) (result i32)
    (call $proxy_call_foreign_function
      (local.get 0) (local.get 1) (local.get 2) (local.get 3) (local.get 4) (local.get 5)))
)
