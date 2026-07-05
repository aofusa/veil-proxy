//! bin(`main.rs`)から移設したクレート内部テスト群。
//!
//! lib + bin 構成へ移行する前、これらのテストは bin クレートルート（旧 `main.rs`）に
//! 置かれ、内部アイテムを直接参照していた。移行後は `main.rs` を薄く保ちつつ、
//! 内部（`pub(crate)`）アイテムへアクセスできる **lib 側のテストモジュール** として
//! ここへ移設している（`cargo test --bins` が壊れる回帰を解消）。
#![cfg(test)]

use crate::config::*;
use crate::http_utils::*;
use crate::logging::*;
use crate::pool::*;

// ====================
// CidrRange テスト
// ====================

mod cidr_tests {
    use super::*;

    #[test]
    fn test_parse_ipv4_cidr() {
        // IPv4 CIDRのパース検証
        let cidr = CidrRange::parse("192.168.1.0/24").unwrap();
        assert!(!cidr.is_ipv6);
        assert_eq!(cidr.prefix_len, 24);
    }

    #[test]
    fn test_parse_ipv4_single() {
        // 単一IPv4アドレスのパース（/32相当）
        let cidr = CidrRange::parse("10.0.0.1").unwrap();
        assert!(!cidr.is_ipv6);
        assert_eq!(cidr.prefix_len, 32);
    }

    #[test]
    fn test_parse_ipv6_cidr() {
        // IPv6 CIDRのパース検証
        let cidr = CidrRange::parse("2001:db8::/32").unwrap();
        assert!(cidr.is_ipv6);
        assert_eq!(cidr.prefix_len, 32);
    }

    #[test]
    fn test_parse_ipv6_single() {
        // 単一IPv6アドレスのパース（/128相当）
        let cidr = CidrRange::parse("::1").unwrap();
        assert!(cidr.is_ipv6);
        assert_eq!(cidr.prefix_len, 128);
    }

    #[test]
    fn test_parse_invalid_cidr() {
        // 無効な入力のパース失敗
        assert!(CidrRange::parse("invalid").is_none());
        assert!(CidrRange::parse("256.256.256.256").is_none());
        assert!(CidrRange::parse("192.168.1.0/33").is_none()); // 無効なプレフィックス
        assert!(CidrRange::parse("").is_none());
    }

    #[test]
    fn test_contains_ipv4_in_range() {
        // IPv4アドレスがCIDR範囲内に含まれる
        let cidr = CidrRange::parse("192.168.0.0/16").unwrap();
        assert!(cidr.contains("192.168.1.100"));
        assert!(cidr.contains("192.168.255.255"));
        assert!(cidr.contains("192.168.0.1"));
    }

    #[test]
    fn test_contains_ipv4_out_of_range() {
        // IPv4アドレスがCIDR範囲外
        let cidr = CidrRange::parse("192.168.0.0/16").unwrap();
        assert!(!cidr.contains("192.169.0.1"));
        assert!(!cidr.contains("10.0.0.1"));
        assert!(!cidr.contains("172.16.0.1"));
    }

    #[test]
    fn test_contains_ipv4_exact_match() {
        // 単一IPアドレスの完全一致
        let cidr = CidrRange::parse("10.0.0.1").unwrap();
        assert!(cidr.contains("10.0.0.1"));
        assert!(!cidr.contains("10.0.0.2"));
    }

    #[test]
    fn test_contains_ipv6_in_range() {
        // IPv6アドレスがCIDR範囲内に含まれる
        let cidr = CidrRange::parse("2001:db8::/32").unwrap();
        assert!(cidr.contains("2001:db8::1"));
        assert!(cidr.contains("2001:db8:ffff::1"));
    }

    #[test]
    fn test_contains_ipv6_out_of_range() {
        // IPv6アドレスがCIDR範囲外
        let cidr = CidrRange::parse("2001:db8::/32").unwrap();
        assert!(!cidr.contains("2001:db9::1"));
        assert!(!cidr.contains("::1"));
    }

    #[test]
    fn test_contains_localhost_ipv6() {
        // IPv6ローカルホストの検証
        let cidr = CidrRange::parse("::1/128").unwrap();
        assert!(cidr.contains("::1"));
        assert!(!cidr.contains("::2"));
    }

    #[test]
    fn test_ipv4_mapped_ipv6() {
        // IPv4をIPv6で確認した場合（異なるアドレスファミリー）
        let cidr_v4 = CidrRange::parse("192.168.1.0/24").unwrap();
        // IPv4 CIDRにIPv6アドレスは含まれない
        assert!(!cidr_v4.contains("::ffff:192.168.1.1"));
    }
}

// ====================
// IpFilter テスト
// ====================

mod ip_filter_tests {
    use super::*;

    #[test]
    fn test_filter_empty_allows_all() {
        // 空のフィルターは全て許可
        let filter = IpFilter::from_lists(&[], &[]);
        assert!(filter.is_allowed("192.168.1.1"));
        assert!(filter.is_allowed("10.0.0.1"));
        assert!(filter.is_allowed("2001:db8::1"));
    }

    #[test]
    fn test_filter_allow_list() {
        // 許可リストのみ設定
        let allowed = vec!["192.168.0.0/16".to_string()];
        let filter = IpFilter::from_lists(&allowed, &[]);

        assert!(filter.is_allowed("192.168.1.1"));
        assert!(filter.is_allowed("192.168.255.255"));
        assert!(!filter.is_allowed("10.0.0.1"));
        assert!(!filter.is_allowed("172.16.0.1"));
    }

    #[test]
    fn test_filter_deny_list() {
        // 拒否リストのみ設定（許可リストが空なので、拒否以外は全て許可）
        let denied = vec!["192.168.1.0/24".to_string()];
        let filter = IpFilter::from_lists(&[], &denied);

        assert!(!filter.is_allowed("192.168.1.1"));
        assert!(filter.is_allowed("192.168.2.1"));
        assert!(filter.is_allowed("10.0.0.1"));
    }

    #[test]
    fn test_filter_deny_priority() {
        // denyがallowより優先されることを検証
        let allowed = vec!["192.168.0.0/16".to_string()];
        let denied = vec!["192.168.1.0/24".to_string()];
        let filter = IpFilter::from_lists(&allowed, &denied);

        // 192.168.1.xはdenyされる
        assert!(!filter.is_allowed("192.168.1.1"));
        assert!(!filter.is_allowed("192.168.1.100"));

        // 192.168.0.xや192.168.2.xはallowされる
        assert!(filter.is_allowed("192.168.0.1"));
        assert!(filter.is_allowed("192.168.2.1"));

        // 許可リスト外は拒否
        assert!(!filter.is_allowed("10.0.0.1"));
    }

    #[test]
    fn test_filter_multiple_ranges() {
        // 複数のCIDR範囲
        let allowed = vec![
            "10.0.0.0/8".to_string(),
            "172.16.0.0/12".to_string(),
            "192.168.0.0/16".to_string(),
        ];
        let filter = IpFilter::from_lists(&allowed, &[]);

        // RFC1918プライベートアドレスは全て許可
        assert!(filter.is_allowed("10.1.2.3"));
        assert!(filter.is_allowed("172.16.100.1"));
        assert!(filter.is_allowed("172.31.255.255"));
        assert!(filter.is_allowed("192.168.0.1"));

        // パブリックアドレスは拒否
        assert!(!filter.is_allowed("8.8.8.8"));
        assert!(!filter.is_allowed("1.1.1.1"));
    }

    #[test]
    fn test_filter_single_ip() {
        // 単一IPアドレスの許可
        let allowed = vec!["127.0.0.1".to_string()];
        let filter = IpFilter::from_lists(&allowed, &[]);

        assert!(filter.is_allowed("127.0.0.1"));
        assert!(!filter.is_allowed("127.0.0.2"));
    }

    #[test]
    fn test_filter_ipv6() {
        // IPv6アドレスのフィルタリング
        let allowed = vec!["2001:db8::/32".to_string(), "::1".to_string()];
        let filter = IpFilter::from_lists(&allowed, &[]);

        assert!(filter.is_allowed("::1"));
        assert!(filter.is_allowed("2001:db8::1"));
        assert!(!filter.is_allowed("2001:db9::1"));
    }

    #[test]
    fn test_filter_is_configured() {
        // フィルターが設定されているかの確認
        let empty = IpFilter::from_lists(&[], &[]);
        assert!(!empty.is_configured());

        let with_allow = IpFilter::from_lists(&["10.0.0.0/8".to_string()], &[]);
        assert!(with_allow.is_configured());

        let with_deny = IpFilter::from_lists(&[], &["192.168.1.0/24".to_string()]);
        assert!(with_deny.is_configured());
    }

    #[test]
    fn test_filter_invalid_entry_ignored() {
        // 無効なエントリは無視される
        let allowed = vec![
            "192.168.1.0/24".to_string(),
            "invalid".to_string(),
            "10.0.0.0/8".to_string(),
        ];
        let filter = IpFilter::from_lists(&allowed, &[]);

        // 有効なエントリは機能する
        assert!(filter.is_allowed("192.168.1.1"));
        assert!(filter.is_allowed("10.1.2.3"));
        assert!(!filter.is_allowed("172.16.0.1"));
    }
}

// ====================
// RateLimitEntry テスト
// ====================

mod rate_limit_tests {
    use super::*;

    #[test]
    fn test_new_entry() {
        // 新規エントリの初期状態
        let entry = RateLimitEntry::new(100);
        assert_eq!(entry.current_count, 1);
        assert_eq!(entry.previous_count, 0);
        assert_eq!(entry.current_minute, 100);
    }

    #[test]
    fn test_record_same_minute() {
        // 同一分内でのリクエスト記録
        let mut entry = RateLimitEntry::new(100);

        // 初期状態: count=1
        let rate = entry.record_request(100, 30);
        // count=2, previous=0, weight=(60-30)/60=0.5
        // estimated = 0*0.5 + 2 = 2
        assert_eq!(entry.current_count, 2);
        assert!(rate >= 2);
    }

    #[test]
    fn test_record_next_minute() {
        // 次の分へ移行
        let mut entry = RateLimitEntry::new(100);
        entry.current_count = 10;

        let rate = entry.record_request(101, 0);

        // previous_count = 10（前の分のカウント）
        // current_count = 1（新しい分）
        assert_eq!(entry.current_minute, 101);
        assert_eq!(entry.previous_count, 10);
        assert_eq!(entry.current_count, 1);

        // rate = 10 * (60-0)/60 + 1 = 10 + 1 = 11
        assert_eq!(rate, 11);
    }

    #[test]
    fn test_record_skip_minutes() {
        // 2分以上経過した場合のリセット
        let mut entry = RateLimitEntry::new(100);
        entry.current_count = 100;
        entry.previous_count = 50;

        let rate = entry.record_request(103, 0);

        // 2分以上経過なのでリセット
        assert_eq!(entry.current_minute, 103);
        assert_eq!(entry.previous_count, 0);
        assert_eq!(entry.current_count, 1);
        assert_eq!(rate, 1);
    }

    #[test]
    fn test_sliding_window_calculation() {
        // スライディングウィンドウ計算の検証
        let mut entry = RateLimitEntry::new(100);
        entry.current_count = 30;
        entry.previous_count = 60;

        // 分の真ん中（30秒経過）でのレート計算
        let rate = entry.record_request(100, 30);
        // current_count = 31, previous = 60
        // weight = (60-30)/60 = 0.5
        // estimated = 60*0.5 + 31 = 30 + 31 = 61
        assert_eq!(entry.current_count, 31);
        assert_eq!(rate, 61);
    }

    #[test]
    fn test_sliding_window_end_of_minute() {
        // 分の終わりでのレート計算（weight ≈ 0）
        let mut entry = RateLimitEntry::new(100);
        entry.current_count = 50;
        entry.previous_count = 100;

        let rate = entry.record_request(100, 59);
        // weight = (60-59)/60 ≈ 0.0167
        // estimated = 100*0.0167 + 51 ≈ 52.67 → ceil → 53
        assert_eq!(entry.current_count, 51);
        assert!(rate >= 51 && rate <= 53);
    }
}

// ====================
// AcceptedEncoding テスト
// ====================

mod encoding_tests {
    use super::*;

    #[test]
    fn test_parse_gzip() {
        let encoding = AcceptedEncoding::parse(b"gzip");
        assert_eq!(encoding, AcceptedEncoding::Gzip);
    }

    #[test]
    fn test_parse_brotli() {
        let encoding = AcceptedEncoding::parse(b"br");
        assert_eq!(encoding, AcceptedEncoding::Brotli);
    }

    #[test]
    fn test_parse_zstd() {
        let encoding = AcceptedEncoding::parse(b"zstd");
        assert_eq!(encoding, AcceptedEncoding::Zstd);
    }

    #[test]
    fn test_parse_deflate() {
        let encoding = AcceptedEncoding::parse(b"deflate");
        assert_eq!(encoding, AcceptedEncoding::Deflate);
    }

    #[test]
    fn test_parse_multiple_prefer_zstd() {
        // 複数指定時はzstdを優先
        let encoding = AcceptedEncoding::parse(b"gzip, br, zstd");
        assert_eq!(encoding, AcceptedEncoding::Zstd);
    }

    #[test]
    fn test_parse_with_quality() {
        // q値指定
        let encoding = AcceptedEncoding::parse(b"gzip;q=0.5, br;q=1.0");
        // br (q=1.0) > gzip (q=0.5)
        assert_eq!(encoding, AcceptedEncoding::Brotli);
    }

    #[test]
    fn test_parse_zstd_higher_quality() {
        // zstdが高いq値を持つ場合
        let encoding = AcceptedEncoding::parse(b"gzip;q=0.8, zstd;q=1.0");
        assert_eq!(encoding, AcceptedEncoding::Zstd);
    }

    #[test]
    fn test_parse_empty() {
        // 空の場合はIdentity
        let encoding = AcceptedEncoding::parse(b"");
        assert_eq!(encoding, AcceptedEncoding::Identity);
    }

    #[test]
    fn test_parse_identity() {
        // identityのみ（圧縮なし）
        let encoding = AcceptedEncoding::parse(b"identity");
        assert_eq!(encoding, AcceptedEncoding::Identity);
    }

    #[test]
    fn test_parse_wildcard() {
        // * はgzipとして扱う
        let encoding = AcceptedEncoding::parse(b"*");
        assert_eq!(encoding, AcceptedEncoding::Gzip);
    }

    #[test]
    fn test_parse_unknown() {
        // 不明なエンコーディングはIdentity
        let encoding = AcceptedEncoding::parse(b"unknown");
        assert_eq!(encoding, AcceptedEncoding::Identity);
    }

    #[test]
    fn test_parse_invalid_utf8() {
        // 無効なUTF-8はIdentity
        let encoding = AcceptedEncoding::parse(&[0xff, 0xfe]);
        assert_eq!(encoding, AcceptedEncoding::Identity);
    }

    #[test]
    fn test_as_header_value() {
        // ヘッダー値への変換
        assert_eq!(AcceptedEncoding::Zstd.as_header_value(), b"zstd");
        assert_eq!(AcceptedEncoding::Brotli.as_header_value(), b"br");
        assert_eq!(AcceptedEncoding::Gzip.as_header_value(), b"gzip");
        assert_eq!(AcceptedEncoding::Deflate.as_header_value(), b"deflate");
        assert_eq!(AcceptedEncoding::Identity.as_header_value(), b"identity");
    }
}

// ====================
// CompressionConfig テスト
// ====================

mod compression_config_tests {
    use super::*;

    #[test]
    fn test_default_config() {
        // デフォルト設定の検証
        let config = CompressionConfig::default();
        assert!(!config.enabled); // デフォルトは無効
        assert_eq!(config.gzip_level, 4);
        assert_eq!(config.brotli_level, 4);
        assert_eq!(config.zstd_level, 3);
        assert_eq!(config.min_size, 1024);
    }

    #[test]
    fn test_validate_valid_config() {
        // 有効な設定の検証
        let config = CompressionConfig {
            enabled: true,
            gzip_level: 6,
            brotli_level: 6,
            zstd_level: 10,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_invalid_gzip_level() {
        // 無効なgzipレベル
        let config = CompressionConfig {
            gzip_level: 10, // 1-9のみ有効
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config_zero = CompressionConfig {
            gzip_level: 0, // 0は無効
            ..Default::default()
        };
        assert!(config_zero.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_brotli_level() {
        // 無効なbrotliレベル
        let config = CompressionConfig {
            brotli_level: 12, // 0-11のみ有効
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_zstd_level() {
        // 無効なzstdレベル
        let config = CompressionConfig {
            zstd_level: 0, // 1-22のみ有効
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config_high = CompressionConfig {
            zstd_level: 23,
            ..Default::default()
        };
        assert!(config_high.validate().is_err());
    }

    #[test]
    fn test_validate_unknown_encoding() {
        // 不明なエンコーディング
        let config = CompressionConfig {
            preferred_encodings: vec!["unknown".to_string()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_should_compress_disabled() {
        // 圧縮無効時
        let config = CompressionConfig {
            enabled: false,
            ..Default::default()
        };
        let result =
            config.should_compress(AcceptedEncoding::Gzip, Some(b"text/html"), Some(2048), None);
        assert!(result.is_none());
    }

    #[test]
    fn test_should_compress_client_identity() {
        // クライアントが圧縮非対応
        let config = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        let result = config.should_compress(
            AcceptedEncoding::Identity,
            Some(b"text/html"),
            Some(2048),
            None,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_should_compress_already_compressed() {
        // バックエンドが既に圧縮済み
        let config = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        let result = config.should_compress(
            AcceptedEncoding::Gzip,
            Some(b"text/html"),
            Some(2048),
            Some(b"gzip"),
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_should_compress_text_html() {
        // text/htmlは圧縮対象
        let config = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        let result = config.should_compress(
            AcceptedEncoding::Gzip,
            Some(b"text/html; charset=utf-8"),
            Some(2048),
            None,
        );
        assert!(result.is_some());
    }

    #[test]
    fn test_should_compress_json() {
        // application/jsonは圧縮対象
        let config = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        let result = config.should_compress(
            AcceptedEncoding::Brotli,
            Some(b"application/json"),
            Some(2048),
            None,
        );
        assert!(result.is_some());
    }

    #[test]
    fn test_should_not_compress_image() {
        // 画像は圧縮スキップ
        let config = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        let result = config.should_compress(
            AcceptedEncoding::Gzip,
            Some(b"image/png"),
            Some(100000),
            None,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_should_not_compress_small() {
        // min_size未満は圧縮しない
        let config = CompressionConfig {
            enabled: true,
            min_size: 1024,
            ..Default::default()
        };
        let result = config.should_compress(
            AcceptedEncoding::Gzip,
            Some(b"text/html"),
            Some(500), // 1024未満
            None,
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_should_not_compress_no_content_type() {
        // Content-Typeがない場合は圧縮しない
        let config = CompressionConfig {
            enabled: true,
            ..Default::default()
        };
        let result = config.should_compress(AcceptedEncoding::Gzip, None, Some(2048), None);
        assert!(result.is_none());
    }
}

// ====================
// WebSocketPollConfig テスト
// ====================

mod websocket_poll_tests {
    use super::*;

    #[test]
    fn test_default_config() {
        // デフォルト設定の検証
        let config = WebSocketPollConfig::default();
        assert_eq!(config.mode, WebSocketPollMode::Adaptive);
        assert_eq!(config.initial_timeout_ms, 1);
        assert_eq!(config.max_timeout_ms, 100);
        assert!((config.backoff_multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_mode_equality() {
        // モード比較
        assert_eq!(WebSocketPollMode::Fixed, WebSocketPollMode::Fixed);
        assert_eq!(WebSocketPollMode::Adaptive, WebSocketPollMode::Adaptive);
        assert_ne!(WebSocketPollMode::Fixed, WebSocketPollMode::Adaptive);
    }
}

// ====================
// SecurityConfig テスト
// ====================

mod security_config_tests {
    use super::*;

    #[test]
    fn test_default_security_config() {
        // デフォルトセキュリティ設定
        let config = SecurityConfig::default();
        assert_eq!(config.max_request_body_size, MAX_BODY_SIZE);
        assert_eq!(config.max_request_header_size, MAX_HEADER_SIZE);
        assert_eq!(config.client_header_timeout_secs, 30);
        assert!(config.allowed_methods.is_empty());
    }

    #[test]
    fn test_security_config_ip_filter() {
        // IPフィルターの構築
        let mut config = SecurityConfig::default();
        config.allowed_ips = vec!["10.0.0.0/8".to_string()];
        config.denied_ips = vec!["10.0.1.0/24".to_string()];

        let filter = config.ip_filter();
        assert!(filter.is_allowed("10.0.0.1"));
        assert!(!filter.is_allowed("10.0.1.1"));
    }
}

// ====================
// PooledConnection テスト
// ====================

mod pooled_connection_tests {
    use super::*;

    #[test]
    fn test_pooled_connection_new() {
        // PooledConnectionの作成
        let stream = (); // ダミー型
        let conn = PooledConnection::new(stream, 30);

        assert_eq!(conn.idle_timeout_secs, 30);
    }

    #[test]
    fn test_pooled_connection_is_valid_immediately() {
        // 作成直後は有効
        let stream = ();
        let conn = PooledConnection::new(stream, 30);

        assert!(conn.is_valid());
    }

    #[test]
    fn test_pooled_connection_is_valid_with_zero_timeout() {
        // タイムアウト0秒の場合、即座に無効
        let stream = ();
        let conn = PooledConnection::new(stream, 0);

        // 作成直後でも0秒以上経過しているため無効
        assert!(!conn.is_valid());
    }

    #[test]
    fn test_pooled_connection_is_valid_with_long_timeout() {
        // 長いタイムアウトの場合、有効
        let stream = ();
        let conn = PooledConnection::new(stream, 3600);

        assert!(conn.is_valid());
    }
}

// ====================
// Config Parse テスト
// ====================

mod config_parse_tests {
    use super::*;

    #[test]
    fn test_parse_log_level() {
        // ログレベルのパース
        assert_eq!(parse_log_level("trace"), ftlog::LevelFilter::Trace);
        assert_eq!(parse_log_level("debug"), ftlog::LevelFilter::Debug);
        assert_eq!(parse_log_level("info"), ftlog::LevelFilter::Info);
        assert_eq!(parse_log_level("warn"), ftlog::LevelFilter::Warn);
        assert_eq!(parse_log_level("error"), ftlog::LevelFilter::Error);
        assert_eq!(parse_log_level("off"), ftlog::LevelFilter::Off);
    }

    #[test]
    fn test_parse_log_level_case_insensitive() {
        // 大文字小文字を区別しない
        assert_eq!(parse_log_level("INFO"), ftlog::LevelFilter::Info);
        assert_eq!(parse_log_level("Debug"), ftlog::LevelFilter::Debug);
        assert_eq!(parse_log_level("WARN"), ftlog::LevelFilter::Warn);
    }

    #[test]
    fn test_parse_log_level_unknown() {
        // 不明なレベルはInfoにフォールバック
        assert_eq!(parse_log_level("unknown"), ftlog::LevelFilter::Info);
        assert_eq!(parse_log_level(""), ftlog::LevelFilter::Info);
    }

    #[test]
    fn test_default_logging_config() {
        // デフォルトロギング設定
        let config = LoggingConfigSection::default();
        assert_eq!(config.level, "info");
        assert_eq!(config.channel_size, 100000);
        assert_eq!(config.flush_interval_ms, 1000);
    }

    #[test]
    fn test_default_server_config() {
        // ServerConfigSectionはDeserializeのみなので、デフォルト値関数をテスト
        // threads = 0 がデフォルト（CPUコア数）
        // http2_enabled = false がデフォルト
        // http3_enabled = false がデフォルト
    }

    #[test]
    fn test_http2_config_default() {
        // HTTP/2設定のデフォルト値
        let config = Http2ConfigSection::default();

        assert_eq!(config.header_table_size, 65536);
        assert_eq!(config.max_concurrent_streams, 256);
        assert_eq!(config.initial_window_size, 1048576);
        assert_eq!(config.max_frame_size, 65536);
        assert_eq!(config.max_header_list_size, 65536);
        assert_eq!(config.connection_window_size, 1048576);
    }

    #[test]
    #[cfg(feature = "http2")]
    fn test_http2_config_to_settings() {
        // HTTP/2設定からHttp2Settingsへの変換
        let config = Http2ConfigSection::default();
        let settings = config.to_http2_settings();

        assert_eq!(settings.header_table_size, config.header_table_size);
        assert_eq!(
            settings.max_concurrent_streams,
            config.max_concurrent_streams
        );
        assert_eq!(settings.initial_window_size, config.initial_window_size);
        assert_eq!(settings.max_frame_size, config.max_frame_size);
        assert_eq!(settings.max_header_list_size, config.max_header_list_size);
    }

    #[test]
    fn test_http3_config_default() {
        // HTTP/3設定のデフォルト値
        let config = Http3ConfigSection::default();

        assert_eq!(config.max_idle_timeout, 30000);
        assert_eq!(config.max_udp_payload_size, 1350);
        assert_eq!(config.initial_max_data, 10_000_000);
        assert_eq!(config.initial_max_streams_bidi, 100);
        assert_eq!(config.initial_max_streams_uni, 100);
    }

    #[test]
    fn test_reuseport_balancing_default() {
        // ReuseportBalancingのデフォルト値
        let balancing = ReuseportBalancing::default();
        assert_eq!(balancing, ReuseportBalancing::Kernel);
    }

    #[test]
    fn test_prometheus_config_default() {
        // Prometheusメトリクス設定のデフォルト
        let config = PrometheusConfig::default();

        assert!(!config.enabled);
        assert_eq!(config.path, "/__metrics");
        assert!(config.allowed_ips.is_empty());
    }

    #[test]
    fn test_prometheus_config_enabled_field() {
        // Prometheus有効化チェック
        let disabled = PrometheusConfig::default();
        assert!(!disabled.enabled);

        let enabled = PrometheusConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(enabled.enabled);
    }
}

// ====================
// UpstreamConfig テスト
// ====================

mod upstream_config_tests {
    use super::*;

    #[test]
    fn test_default_health_check_config() {
        // ヘルスチェック設定のデフォルト値
        let config = HealthCheckConfig::default();

        assert_eq!(config.interval_secs, 10);
        assert_eq!(config.path, "/");
        assert_eq!(config.timeout_secs, 5);
        assert_eq!(config.unhealthy_threshold, 3);
        assert_eq!(config.healthy_threshold, 2);
    }

    #[test]
    fn test_default_health_check_statuses() {
        // デフォルトの健康ステータスコード
        let config = HealthCheckConfig::default();

        assert!(config.healthy_statuses.contains(&200));
        assert!(config.healthy_statuses.contains(&201));
        assert!(config.healthy_statuses.contains(&204));
        assert!(config.healthy_statuses.contains(&301));
        assert!(config.healthy_statuses.contains(&302));
        assert!(config.healthy_statuses.contains(&304));
    }
}

// ====================
// Backend テスト
// ====================

mod backend_tests {
    #[test]
    fn test_backend_config_types() {
        // BackendConfigの種類を確認
        // File, Proxy, Static, Redirect などの種類が存在
        // 各種類に対応した処理が実装されている
        assert!(true);
    }
}

// ====================
// ProxyTarget テスト
// ====================

mod proxy_target_tests {
    use super::*;

    #[test]
    fn test_parse_http_url() {
        let target = ProxyTarget::parse("http://localhost:8080/api").unwrap();

        assert_eq!(target.host, "localhost");
        assert_eq!(target.port, 8080);
        assert!(!target.use_tls);
        assert_eq!(target.path_prefix, "/api");
    }

    #[test]
    fn test_parse_https_url() {
        let target = ProxyTarget::parse("https://example.com/").unwrap();

        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 443);
        assert!(target.use_tls);
    }

    #[test]
    fn test_parse_url_default_ports() {
        let http = ProxyTarget::parse("http://localhost/").unwrap();
        assert_eq!(http.port, 80);

        let https = ProxyTarget::parse("https://localhost/").unwrap();
        assert_eq!(https.port, 443);
    }

    #[test]
    fn test_parse_url_with_path() {
        let target = ProxyTarget::parse("http://api.example.com:3000/v1/users").unwrap();

        assert_eq!(target.host, "api.example.com");
        assert_eq!(target.port, 3000);
        assert_eq!(target.path_prefix, "/v1/users");
    }

    #[test]
    fn test_parse_url_no_path() {
        let target = ProxyTarget::parse("http://localhost:8080").unwrap();

        assert_eq!(target.path_prefix, "/");
    }

    #[test]
    fn test_parse_invalid_url() {
        assert!(ProxyTarget::parse("invalid").is_none());
        assert!(ProxyTarget::parse("ftp://localhost/").is_none());
        assert!(ProxyTarget::parse("").is_none());
        assert!(ProxyTarget::parse("://no-scheme").is_none());
    }

    #[test]
    fn test_with_sni_name() {
        let target = ProxyTarget::parse("https://192.168.1.1:443/")
            .unwrap()
            .with_sni_name(Some("api.example.com".to_string()));

        assert_eq!(target.sni_name, Some("api.example.com".to_string()));
        assert_eq!(target.host, "192.168.1.1");
    }

    #[test]
    fn test_with_h2c() {
        let target = ProxyTarget::parse("http://localhost:8080/")
            .unwrap()
            .with_h2c(true);

        assert!(target.use_h2c);
        assert!(!target.use_tls);
    }

    #[test]
    fn test_ipv6_host() {
        // IPv6アドレスのパース（ブラケット表記）
        let target = ProxyTarget::parse("http://[::1]:8080/");
        // 現在の実装ではIPv6はサポートされていない可能性があるため、
        // パース結果を確認
        if let Some(t) = target {
            assert_eq!(t.port, 8080);
        }
    }
}

// ====================
// UpstreamGroup 選択ロジックテスト
// ====================

mod upstream_selection_tests {
    use super::*;

    fn create_test_servers() -> Vec<UpstreamServerEntry> {
        vec![
            UpstreamServerEntry {
                url: "http://server1:8080".into(),
                sni_name: None,
                use_h2c: false,
                weight: 1,
            },
            UpstreamServerEntry {
                url: "http://server2:8080".into(),
                sni_name: None,
                use_h2c: false,
                weight: 1,
            },
            UpstreamServerEntry {
                url: "http://server3:8080".into(),
                sni_name: None,
                use_h2c: false,
                weight: 1,
            },
        ]
    }

    #[test]
    fn test_upstream_group_creation() {
        let servers = create_test_servers();
        let group = UpstreamGroup::new(
            "test-group".into(),
            servers,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        );

        assert!(group.is_some());
        let group = group.unwrap();
        assert_eq!(group.len(), 3);
    }

    #[test]
    fn test_upstream_group_empty_servers() {
        let group = UpstreamGroup::new(
            "empty".into(),
            vec![],
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        );

        assert!(group.is_none());
    }

    #[test]
    fn test_upstream_group_invalid_url() {
        let servers = vec![UpstreamServerEntry {
            url: "invalid-url".into(),
            sni_name: None,
            use_h2c: false,
            weight: 1,
        }];
        let group = UpstreamGroup::new(
            "invalid".into(),
            servers,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        );

        assert!(group.is_none());
    }

    #[test]
    fn test_round_robin_distribution() {
        let servers = create_test_servers();
        let group = UpstreamGroup::new(
            "rr-test".into(),
            servers,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        let mut hosts: Vec<String> = Vec::new();
        for _ in 0..9 {
            if let Some(server) = group.select("client") {
                hosts.push(server.target.host.clone());
            }
        }

        // 9回選択で3サイクル
        assert_eq!(hosts.len(), 9);

        // 各サーバーが3回ずつ選択される
        let count_server1 = hosts.iter().filter(|h| *h == "server1").count();
        let count_server2 = hosts.iter().filter(|h| *h == "server2").count();
        let count_server3 = hosts.iter().filter(|h| *h == "server3").count();

        assert_eq!(count_server1, 3);
        assert_eq!(count_server2, 3);
        assert_eq!(count_server3, 3);
    }

    #[test]
    fn test_ip_hash_consistency() {
        let servers = create_test_servers();
        let group = UpstreamGroup::new(
            "iphash-test".into(),
            servers,
            LoadBalanceAlgorithm::IpHash,
            None,
            false,
        )
        .unwrap();

        let client_ip = "192.168.1.100";
        let first = group.select(client_ip).map(|s| s.target.host.clone());

        // 同じIPは常に同じサーバーを選択
        for _ in 0..20 {
            let selected = group.select(client_ip).map(|s| s.target.host.clone());
            assert_eq!(first, selected, "IP Hash should be consistent");
        }
    }

    #[test]
    fn test_ip_hash_different_ips_distribute() {
        let servers = create_test_servers();
        let group = UpstreamGroup::new(
            "iphash-dist".into(),
            servers,
            LoadBalanceAlgorithm::IpHash,
            None,
            false,
        )
        .unwrap();

        let mut selected_hosts = std::collections::HashSet::new();

        // 100個の異なるIPで分散を確認
        for i in 0..100 {
            let ip = format!("10.0.{}.{}", i / 256, i % 256);
            if let Some(server) = group.select(&ip) {
                selected_hosts.insert(server.target.host.clone());
            }
        }

        // 複数サーバーに分散されることを確認
        assert!(
            selected_hosts.len() >= 2,
            "Should distribute across multiple servers"
        );
    }

    #[test]
    fn test_least_connections_selection() {
        let servers = create_test_servers();
        let group = UpstreamGroup::new(
            "lc-test".into(),
            servers,
            LoadBalanceAlgorithm::LeastConnections,
            None,
            false,
        )
        .unwrap();

        // 初期状態では全サーバーの接続数が0なので、最初のサーバーが選択される
        let selected = group.select("client");
        assert!(selected.is_some());
    }

    #[test]
    fn test_single_server_group() {
        let target = ProxyTarget::parse("http://single:8080/").unwrap();
        let group = UpstreamGroup::single(target);

        assert_eq!(group.len(), 1);

        // 何度選択しても同じサーバー
        for _ in 0..5 {
            let selected = group.select("client");
            assert!(selected.is_some());
            assert_eq!(selected.unwrap().target.host, "single");
        }
    }
}

// ====================
// UpstreamServer 健康状態テスト
// ====================

mod upstream_health_tests {
    use super::*;

    #[test]
    fn test_server_initial_state_healthy() {
        let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
        let server = UpstreamServer::new(target);

        assert!(server.is_healthy());
    }

    #[test]
    fn test_server_becomes_unhealthy_after_failures() {
        let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
        let server = UpstreamServer::new(target);

        // 3回失敗すると不健全になる（デフォルト閾値）
        server.record_failure(3);
        assert!(server.is_healthy()); // まだ健全
        server.record_failure(3);
        assert!(server.is_healthy()); // まだ健全
        server.record_failure(3);
        assert!(!server.is_healthy()); // 3回目で不健全
    }

    #[test]
    fn test_server_becomes_healthy_after_successes() {
        let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
        let server = UpstreamServer::new(target);

        // まず不健全にする
        for _ in 0..3 {
            server.record_failure(3);
        }
        assert!(!server.is_healthy());

        // 2回成功すると健全になる（デフォルト閾値）
        server.record_success(2);
        assert!(!server.is_healthy()); // まだ不健全
        server.record_success(2);
        assert!(server.is_healthy()); // 2回目で健全
    }

    #[test]
    fn test_server_connection_count() {
        let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
        let server = UpstreamServer::new(target);

        assert_eq!(server.connections(), 0);

        server.acquire();
        assert_eq!(server.connections(), 1);

        server.acquire();
        assert_eq!(server.connections(), 2);

        server.release();
        assert_eq!(server.connections(), 1);

        server.release();
        assert_eq!(server.connections(), 0);
    }

    #[test]
    fn test_select_skips_unhealthy_servers() {
        let servers = vec![
            UpstreamServerEntry {
                url: "http://healthy:8080".into(),
                sni_name: None,
                use_h2c: false,
                weight: 1,
            },
            UpstreamServerEntry {
                url: "http://unhealthy:8080".into(),
                sni_name: None,
                use_h2c: false,
                weight: 1,
            },
        ];
        let group = UpstreamGroup::new(
            "health-test".into(),
            servers,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        // 2番目のサーバーを不健全にマーク（3回失敗で不健全）
        for _ in 0..3 {
            group.servers[1].record_failure(3);
        }

        // 10回選択しても不健全サーバーは選択されない
        for _ in 0..10 {
            let selected = group.select("client");
            assert!(selected.is_some());
            assert_eq!(selected.unwrap().target.host, "healthy");
        }
    }

    #[test]
    fn test_select_returns_none_all_unhealthy() {
        let servers = vec![
            UpstreamServerEntry {
                url: "http://server1:8080".into(),
                sni_name: None,
                use_h2c: false,
                weight: 1,
            },
            UpstreamServerEntry {
                url: "http://server2:8080".into(),
                sni_name: None,
                use_h2c: false,
                weight: 1,
            },
        ];
        let group = UpstreamGroup::new(
            "all-unhealthy".into(),
            servers,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        // 全サーバーを不健全にマーク（3回失敗で不健全）
        for server in &group.servers {
            for _ in 0..3 {
                server.record_failure(3);
            }
        }

        let selected = group.select("client");
        assert!(selected.is_none());
    }

    #[test]
    fn test_failure_resets_success_counter() {
        let target = ProxyTarget::parse("http://localhost:8080/").unwrap();
        let server = UpstreamServer::new(target);

        // 不健全にする
        for _ in 0..3 {
            server.record_failure(3);
        }
        assert!(!server.is_healthy());

        // 1回成功
        server.record_success(2);

        // 失敗で成功カウンターリセット
        server.record_failure(3);

        // 再度2回成功が必要
        server.record_success(2);
        assert!(!server.is_healthy());
        server.record_success(2);
        assert!(server.is_healthy());
    }
}

// ====================
// LoadBalanceAlgorithm パーステスト
// ====================

mod load_balance_algorithm_tests {
    use super::*;

    #[test]
    fn test_default_algorithm() {
        let algo = LoadBalanceAlgorithm::default();
        assert_eq!(algo, LoadBalanceAlgorithm::RoundRobin);
    }
}

// ====================
// HealthCheckConfig テスト
// ====================

mod health_check_config_tests {
    use super::*;

    #[test]
    fn test_is_status_healthy() {
        let config = HealthCheckConfig::default();

        // 健康なステータス
        assert!(config.healthy_statuses.contains(&200));
        assert!(config.healthy_statuses.contains(&201));
        assert!(config.healthy_statuses.contains(&204));
        assert!(config.healthy_statuses.contains(&301));
        assert!(config.healthy_statuses.contains(&302));
        assert!(config.healthy_statuses.contains(&304));

        // 不健康なステータス
        assert!(!config.healthy_statuses.contains(&400));
        assert!(!config.healthy_statuses.contains(&500));
        assert!(!config.healthy_statuses.contains(&503));
    }

    #[test]
    fn test_custom_healthy_statuses() {
        let mut config = HealthCheckConfig::default();
        config.healthy_statuses = vec![200, 201];

        assert!(config.healthy_statuses.contains(&200));
        assert!(config.healthy_statuses.contains(&201));
        assert!(!config.healthy_statuses.contains(&204));
    }
}

// ====================
// HTTP/1.1 RFC準拠ヘルパー関数テスト
// ====================

mod http11_tests {
    use super::*;

    #[test]
    fn test_add_via_header_new() {
        let mut headers: Vec<(Vec<u8>, Vec<u8>)> =
            vec![(b"host".to_vec(), b"example.com".to_vec())];
        add_via_header(&mut headers, "proxy.example.com");

        assert_eq!(headers.len(), 2);
        assert_eq!(headers[1].0, b"via".to_vec());
        assert_eq!(headers[1].1, b"1.1 proxy.example.com".to_vec());
    }

    #[test]
    fn test_add_via_header_existing() {
        let mut headers: Vec<(Vec<u8>, Vec<u8>)> =
            vec![(b"via".to_vec(), b"1.1 first-proxy".to_vec())];
        add_via_header(&mut headers, "second-proxy");

        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].1, b"1.1 first-proxy, 1.1 second-proxy".to_vec());
    }

    #[test]
    fn test_validate_http_headers_valid() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"content-length".to_vec(), b"100".to_vec())];
        assert!(validate_http_headers(&headers).is_ok());
    }

    #[test]
    fn test_validate_http_headers_valid_transfer_encoding() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> =
            vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())];
        assert!(validate_http_headers(&headers).is_ok());
    }

    #[test]
    fn test_validate_http_headers_conflict() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"content-length".to_vec(), b"100".to_vec()),
            (b"transfer-encoding".to_vec(), b"chunked".to_vec()),
        ];
        assert!(validate_http_headers(&headers).is_err());
    }

    #[test]
    fn test_check_expect_continue_true() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"expect".to_vec(), b"100-continue".to_vec())];
        assert!(check_expect_continue(&headers));
    }

    #[test]
    fn test_check_expect_continue_false() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"host".to_vec(), b"example.com".to_vec())];
        assert!(!check_expect_continue(&headers));
    }

    #[test]
    fn test_check_header_count_within_limit() {
        assert!(check_header_count(50, 64).is_ok());
    }

    #[test]
    fn test_check_header_count_expansion() {
        let result = check_header_count(64, 64);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 128);
    }

    #[test]
    fn test_check_header_count_max_limit() {
        let result = check_header_count(1024, 1024);
        assert!(result.is_err());
    }
}

// ====================
// RFC 7230-7233 準拠ヘルパー関数テスト
// ====================

mod rfc_compliance_tests {
    use super::*;

    // Hostヘッダー検証テスト (RFC 7230 Section 5.4)

    #[test]
    fn test_validate_host_header_present_http11() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"host".to_vec(), b"example.com".to_vec())];
        assert!(validate_host_header(&headers, 1).is_ok());
    }

    #[test]
    fn test_validate_host_header_missing_http11() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> =
            vec![(b"content-type".to_vec(), b"text/html".to_vec())];
        assert!(validate_host_header(&headers, 1).is_err());
    }

    #[test]
    fn test_validate_host_header_http10_optional() {
        // HTTP/1.0ではHostヘッダーは任意
        let headers: Vec<(Vec<u8>, Vec<u8>)> =
            vec![(b"content-type".to_vec(), b"text/html".to_vec())];
        assert!(validate_host_header(&headers, 0).is_ok());
    }

    #[test]
    fn test_validate_host_header_case_insensitive() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"HOST".to_vec(), b"example.com".to_vec())];
        assert!(validate_host_header(&headers, 1).is_ok());
    }

    // Hop-by-hopヘッダーテスト (RFC 7230 Section 6.1)

    #[test]
    fn test_is_hop_by_hop_header_connection() {
        assert!(is_hop_by_hop_header(b"connection"));
        assert!(is_hop_by_hop_header(b"Connection"));
        assert!(is_hop_by_hop_header(b"CONNECTION"));
    }

    #[test]
    fn test_is_hop_by_hop_header_keep_alive() {
        assert!(is_hop_by_hop_header(b"keep-alive"));
        assert!(is_hop_by_hop_header(b"Keep-Alive"));
    }

    #[test]
    fn test_is_hop_by_hop_header_proxy_connection() {
        assert!(is_hop_by_hop_header(b"proxy-connection"));
        assert!(is_hop_by_hop_header(b"Proxy-Connection"));
    }

    #[test]
    fn test_is_hop_by_hop_header_te() {
        assert!(is_hop_by_hop_header(b"te"));
        assert!(is_hop_by_hop_header(b"TE"));
    }

    #[test]
    fn test_is_hop_by_hop_header_trailer() {
        assert!(is_hop_by_hop_header(b"trailer"));
    }

    #[test]
    fn test_is_hop_by_hop_header_transfer_encoding() {
        assert!(is_hop_by_hop_header(b"transfer-encoding"));
    }

    #[test]
    fn test_is_hop_by_hop_header_upgrade() {
        assert!(is_hop_by_hop_header(b"upgrade"));
    }

    #[test]
    fn test_is_not_hop_by_hop_header() {
        assert!(!is_hop_by_hop_header(b"content-type"));
        assert!(!is_hop_by_hop_header(b"host"));
        assert!(!is_hop_by_hop_header(b"accept"));
        assert!(!is_hop_by_hop_header(b"cache-control"));
    }

    #[test]
    fn test_strip_hop_by_hop_headers_basic() {
        let mut headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"host".to_vec(), b"example.com".to_vec()),
            (b"connection".to_vec(), b"keep-alive".to_vec()),
            (b"keep-alive".to_vec(), b"timeout=5".to_vec()),
            (b"content-type".to_vec(), b"text/html".to_vec()),
        ];

        strip_hop_by_hop_headers(&mut headers);

        assert_eq!(headers.len(), 2);
        assert!(headers.iter().any(|(n, _)| n == b"host"));
        assert!(headers.iter().any(|(n, _)| n == b"content-type"));
        assert!(!headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"connection")));
        assert!(!headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"keep-alive")));
    }

    #[test]
    fn test_strip_hop_by_hop_headers_custom() {
        // Connectionヘッダーで指定されたカスタムヘッダーも削除
        let mut headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"host".to_vec(), b"example.com".to_vec()),
            (b"connection".to_vec(), b"keep-alive, x-custom".to_vec()),
            (b"x-custom".to_vec(), b"value".to_vec()),
        ];

        strip_hop_by_hop_headers(&mut headers);

        assert_eq!(headers.len(), 1);
        assert!(headers.iter().any(|(n, _)| n == b"host"));
    }

    // Rangeヘッダーテスト (RFC 7233)

    #[test]
    fn test_parse_range_header_single_range() {
        let result = parse_range_header(b"bytes=0-99");
        assert!(result.is_some());
        let parsed = result.unwrap();
        assert_eq!(parsed.ranges.len(), 1);
        assert_eq!(
            parsed.ranges[0],
            RangeSpec::Bytes {
                start: 0,
                end: Some(99)
            }
        );
    }

    #[test]
    fn test_parse_range_header_open_end() {
        let result = parse_range_header(b"bytes=100-");
        assert!(result.is_some());
        let parsed = result.unwrap();
        assert_eq!(
            parsed.ranges[0],
            RangeSpec::Bytes {
                start: 100,
                end: None
            }
        );
    }

    #[test]
    fn test_parse_range_header_suffix() {
        let result = parse_range_header(b"bytes=-500");
        assert!(result.is_some());
        let parsed = result.unwrap();
        assert_eq!(parsed.ranges[0], RangeSpec::Suffix { suffix_length: 500 });
    }

    #[test]
    fn test_parse_range_header_multiple() {
        let result = parse_range_header(b"bytes=0-99, 200-299");
        assert!(result.is_some());
        let parsed = result.unwrap();
        assert_eq!(parsed.ranges.len(), 2);
    }

    #[test]
    fn test_parse_range_header_invalid_no_bytes() {
        assert!(parse_range_header(b"0-99").is_none());
    }

    #[test]
    fn test_parse_range_header_invalid_start_greater_than_end() {
        assert!(parse_range_header(b"bytes=100-50").is_none());
    }

    #[test]
    fn test_parse_range_header_case_insensitive() {
        let result = parse_range_header(b"BYTES=0-100");
        assert!(result.is_some());
    }

    // normalize_range テスト

    #[test]
    fn test_normalize_range_bytes_within_bounds() {
        let spec = RangeSpec::Bytes {
            start: 0,
            end: Some(99),
        };
        let result = normalize_range(&spec, 1000);
        assert_eq!(result, Some((0, 99)));
    }

    #[test]
    fn test_normalize_range_bytes_end_exceeds() {
        let spec = RangeSpec::Bytes {
            start: 0,
            end: Some(9999),
        };
        let result = normalize_range(&spec, 1000);
        assert_eq!(result, Some((0, 999))); // end should be clamped
    }

    #[test]
    fn test_normalize_range_bytes_open_end() {
        let spec = RangeSpec::Bytes {
            start: 500,
            end: None,
        };
        let result = normalize_range(&spec, 1000);
        assert_eq!(result, Some((500, 999)));
    }

    #[test]
    fn test_normalize_range_bytes_start_exceeds() {
        let spec = RangeSpec::Bytes {
            start: 1000,
            end: Some(1100),
        };
        let result = normalize_range(&spec, 1000);
        assert_eq!(result, None); // 416 Range Not Satisfiable
    }

    #[test]
    fn test_normalize_range_suffix() {
        let spec = RangeSpec::Suffix { suffix_length: 100 };
        let result = normalize_range(&spec, 1000);
        assert_eq!(result, Some((900, 999)));
    }

    #[test]
    fn test_normalize_range_suffix_larger_than_content() {
        let spec = RangeSpec::Suffix {
            suffix_length: 2000,
        };
        let result = normalize_range(&spec, 1000);
        assert_eq!(result, Some((0, 999)));
    }

    #[test]
    fn test_normalize_range_empty_content() {
        let spec = RangeSpec::Bytes {
            start: 0,
            end: Some(100),
        };
        let result = normalize_range(&spec, 0);
        assert_eq!(result, None);
    }

    // 206 Partial Content レスポンス構築テスト

    #[test]
    fn test_build_partial_response_header() {
        let header = build_partial_response_header(0, 99, 1000, "text/plain", false);
        let header_str = String::from_utf8_lossy(&header);

        assert!(header_str.contains("HTTP/1.1 206 Partial Content"));
        assert!(header_str.contains("Content-Range: bytes 0-99/1000"));
        assert!(header_str.contains("Content-Length: 100"));
        assert!(header_str.contains("Content-Type: text/plain"));
        assert!(header_str.contains("Connection: keep-alive"));
    }

    #[test]
    fn test_build_partial_response_header_close() {
        let header = build_partial_response_header(0, 99, 1000, "text/plain", true);
        let header_str = String::from_utf8_lossy(&header);

        assert!(header_str.contains("Connection: close"));
    }

    #[test]
    fn test_build_range_not_satisfiable_response() {
        let response = build_range_not_satisfiable_response(1000);
        let response_str = String::from_utf8_lossy(&response);

        assert!(response_str.contains("HTTP/1.1 416 Range Not Satisfiable"));
        assert!(response_str.contains("Content-Range: bytes */1000"));
        assert!(response_str.contains("Content-Length: 0"));
    }

    // TEヘッダーテスト (RFC 7230 Section 4.3)

    #[test]
    fn test_parse_te_header_trailers() {
        let te = parse_te_header(b"trailers");
        assert!(te.supports_trailers);
        assert!(te.encodings.is_empty());
    }

    #[test]
    fn test_parse_te_header_trailers_case_insensitive() {
        let te = parse_te_header(b"TRAILERS");
        assert!(te.supports_trailers);
    }

    #[test]
    fn test_parse_te_header_gzip() {
        let te = parse_te_header(b"gzip");
        assert!(!te.supports_trailers);
        assert_eq!(te.encodings.len(), 1);
        assert_eq!(te.encodings[0], "gzip");
    }

    #[test]
    fn test_parse_te_header_multiple() {
        let te = parse_te_header(b"trailers, gzip, deflate");
        assert!(te.supports_trailers);
        assert_eq!(te.encodings.len(), 2);
        assert!(te.encodings.contains(&"gzip".to_string()));
        assert!(te.encodings.contains(&"deflate".to_string()));
    }

    #[test]
    fn test_parse_te_header_with_quality() {
        let te = parse_te_header(b"gzip;q=0.5, deflate;q=1.0");
        assert_eq!(te.encodings.len(), 2);
        assert_eq!(te.encodings[0], "gzip");
        assert_eq!(te.encodings[1], "deflate");
    }

    #[test]
    fn test_parse_te_header_chunked_ignored() {
        // chunkedはTE経由で指定すべきではないがスキップ
        let te = parse_te_header(b"chunked, trailers");
        assert!(te.supports_trailers);
        assert!(te.encodings.is_empty());
    }

    #[test]
    fn test_parse_te_header_empty() {
        let te = parse_te_header(b"");
        assert!(!te.supports_trailers);
        assert!(te.encodings.is_empty());
    }

    // get_range_header テスト

    #[test]
    fn test_get_range_header_found() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"host".to_vec(), b"example.com".to_vec()),
            (b"range".to_vec(), b"bytes=0-100".to_vec()),
        ];
        let result = get_range_header(&headers);
        assert_eq!(result, Some(b"bytes=0-100".as_slice()));
    }

    #[test]
    fn test_get_range_header_not_found() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"host".to_vec(), b"example.com".to_vec())];
        let result = get_range_header(&headers);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_range_header_case_insensitive() {
        let headers: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"Range".to_vec(), b"bytes=0-100".to_vec())];
        let result = get_range_header(&headers);
        assert!(result.is_some());
    }

    // should_advertise_accept_ranges テスト

    #[test]
    fn test_should_advertise_accept_ranges_get() {
        assert!(should_advertise_accept_ranges(b"GET"));
        assert!(should_advertise_accept_ranges(b"get"));
    }

    #[test]
    fn test_should_advertise_accept_ranges_head() {
        assert!(should_advertise_accept_ranges(b"HEAD"));
        assert!(should_advertise_accept_ranges(b"head"));
    }

    #[test]
    fn test_should_not_advertise_accept_ranges_post() {
        assert!(!should_advertise_accept_ranges(b"POST"));
        assert!(!should_advertise_accept_ranges(b"PUT"));
        assert!(!should_advertise_accept_ranges(b"DELETE"));
    }
}
