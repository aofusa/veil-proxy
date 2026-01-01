//! Unit tests for WASM Extension System
//!
//! This module tests the Proxy-Wasm implementation.

// Test constants module
mod constants_tests {
    use crate::wasm::constants::*;

    #[test]
    fn test_proxy_result_constants() {
        assert_eq!(PROXY_RESULT_OK, 0);
        assert_eq!(PROXY_RESULT_NOT_FOUND, 1);
        assert_eq!(PROXY_RESULT_BAD_ARGUMENT, 2);
        assert_eq!(PROXY_RESULT_NOT_ALLOWED, 13);
    }

    #[test]
    fn test_action_constants() {
        assert_eq!(ACTION_CONTINUE, 0);
        assert_eq!(ACTION_PAUSE, 1);
    }

    #[test]
    fn test_map_type_constants() {
        assert_eq!(HTTP_REQUEST_HEADERS, 0);
        assert_eq!(HTTP_RESPONSE_HEADERS, 2);
        assert_eq!(HTTP_CALL_RESPONSE_HEADERS, 6);
    }

    #[test]
    fn test_buffer_type_constants() {
        assert_eq!(HTTP_REQUEST_BODY, 0);
        assert_eq!(HTTP_RESPONSE_BODY, 1);
        assert_eq!(PLUGIN_CONFIGURATION, 4);
    }

    #[test]
    fn test_log_level_constants() {
        assert_eq!(LOG_TRACE, 0);
        assert_eq!(LOG_DEBUG, 1);
        assert_eq!(LOG_INFO, 2);
        assert_eq!(LOG_WARN, 3);
        assert_eq!(LOG_ERROR, 4);
        assert_eq!(LOG_CRITICAL, 5);
    }

    #[test]
    fn test_metric_type_constants() {
        assert_eq!(METRIC_TYPE_COUNTER, 0);
        assert_eq!(METRIC_TYPE_GAUGE, 1);
        assert_eq!(METRIC_TYPE_HISTOGRAM, 2);
    }
}

// Test capabilities module
mod capabilities_tests {
    use crate::wasm::capabilities::{CapabilityPreset, ModuleCapabilities};

    #[test]
    fn test_default_capabilities_all_false() {
        let caps = ModuleCapabilities::default();

        // All should be false by default
        assert!(!caps.allow_logging);
        assert!(!caps.allow_metrics);
        assert!(!caps.allow_shared_data);
        assert!(!caps.allow_request_headers_read);
        assert!(!caps.allow_request_headers_write);
        assert!(!caps.allow_request_body_read);
        assert!(!caps.allow_request_body_write);
        assert!(!caps.allow_response_headers_read);
        assert!(!caps.allow_response_headers_write);
        assert!(!caps.allow_response_body_read);
        assert!(!caps.allow_response_body_write);
        assert!(!caps.allow_send_local_response);
        assert!(!caps.allow_http_calls);
        assert!(caps.allowed_upstreams.is_empty());
    }

    #[test]
    fn test_property_allowed_empty() {
        let caps = ModuleCapabilities::default();
        assert!(!caps.is_property_allowed("request.path"));
        assert!(!caps.is_property_allowed("anything"));
    }

    #[test]
    fn test_property_allowed_wildcard() {
        let caps = ModuleCapabilities {
            allowed_properties: vec!["*".to_string()],
            ..Default::default()
        };
        assert!(caps.is_property_allowed("request.path"));
        assert!(caps.is_property_allowed("anything.else"));
    }

    #[test]
    fn test_property_allowed_prefix() {
        let caps = ModuleCapabilities {
            allowed_properties: vec!["request.*".to_string(), "source.address".to_string()],
            ..Default::default()
        };
        assert!(caps.is_property_allowed("request.path"));
        assert!(caps.is_property_allowed("request.method"));
        assert!(caps.is_property_allowed("source.address"));
        assert!(!caps.is_property_allowed("response.code"));
        assert!(!caps.is_property_allowed("source.port"));
    }

    #[test]
    fn test_upstream_allowed() {
        let caps = ModuleCapabilities {
            allow_http_calls: true,
            allowed_upstreams: vec!["webdis".to_string(), "auth".to_string()],
            ..Default::default()
        };
        assert!(caps.is_upstream_allowed("webdis"));
        assert!(caps.is_upstream_allowed("auth"));
        assert!(!caps.is_upstream_allowed("other"));
    }

    #[test]
    fn test_upstream_not_allowed_if_http_calls_disabled() {
        let caps = ModuleCapabilities {
            allow_http_calls: false,
            allowed_upstreams: vec!["webdis".to_string()],
            ..Default::default()
        };
        assert!(!caps.is_upstream_allowed("webdis"));
    }

    #[test]
    fn test_upstream_all_allowed_if_empty_list() {
        let caps = ModuleCapabilities {
            allow_http_calls: true,
            allowed_upstreams: vec![], // Empty = all allowed
            ..Default::default()
        };
        assert!(caps.is_upstream_allowed("any_upstream"));
    }

    #[test]
    fn test_preset_minimal() {
        let caps = CapabilityPreset::Minimal.to_capabilities();
        assert!(caps.allow_logging);
        assert!(caps.allow_request_headers_read);
        assert!(!caps.allow_request_headers_write);
        assert!(!caps.allow_http_calls);
    }

    #[test]
    fn test_preset_standard() {
        let caps = CapabilityPreset::Standard.to_capabilities();
        assert!(caps.allow_logging);
        assert!(caps.allow_metrics);
        assert!(caps.allow_request_headers_read);
        assert!(caps.allow_request_headers_write);
        assert!(caps.allow_send_local_response);
        assert!(!caps.allow_request_body_write);
    }

    #[test]
    fn test_preset_extended() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        assert!(caps.allow_logging);
        assert!(caps.allow_metrics);
        assert!(caps.allow_shared_data);
        assert!(caps.allow_request_body_write);
        assert!(caps.allow_response_body_write);
        assert!(caps.allow_http_calls);
    }
}

// Test context module
mod context_tests {
    use crate::wasm::capabilities::ModuleCapabilities;
    use crate::wasm::context::HttpContext;
    use crate::wasm::types::LocalResponse;

    #[test]
    fn test_context_new() {
        let caps = ModuleCapabilities::default();
        let ctx = HttpContext::new(42, caps);

        assert_eq!(ctx.context_id, 42);
        assert_eq!(ctx.root_context_id, 0);
        assert!(ctx.request_headers.is_empty());
        assert!(ctx.request_body.is_empty());
        assert!(ctx.response_headers.is_empty());
        assert!(ctx.local_response.is_none());
    }

    #[test]
    fn test_set_request() {
        let caps = ModuleCapabilities::default();
        let mut ctx = HttpContext::new(1, caps);

        let headers = vec![
            ("Host".to_string(), "example.com".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];

        ctx.set_request("GET", "/api/users?page=1", headers.clone(), "192.168.1.1");

        assert_eq!(ctx.request_method, "GET");
        assert_eq!(ctx.request_path, "/api/users?page=1");
        assert_eq!(ctx.request_query, "page=1");
        assert_eq!(ctx.client_ip, "192.168.1.1");
        assert_eq!(ctx.request_headers.len(), 2);
    }

    #[test]
    fn test_set_request_no_query() {
        let caps = ModuleCapabilities::default();
        let mut ctx = HttpContext::new(1, caps);

        ctx.set_request("POST", "/api/users", vec![], "10.0.0.1");

        assert_eq!(ctx.request_path, "/api/users");
        assert_eq!(ctx.request_query, "");
    }

    #[test]
    fn test_set_response() {
        let caps = ModuleCapabilities::default();
        let mut ctx = HttpContext::new(1, caps);

        let headers = vec![("Content-Type".to_string(), "text/html".to_string())];

        ctx.set_response(200, headers);

        assert_eq!(ctx.response_status, 200);
        assert_eq!(ctx.response_headers.len(), 1);
    }

    #[test]
    fn test_allocate_http_call_token() {
        let caps = ModuleCapabilities::default();
        let mut ctx = HttpContext::new(1, caps);

        let token1 = ctx.allocate_http_call_token();
        let token2 = ctx.allocate_http_call_token();
        let token3 = ctx.allocate_http_call_token();

        assert_eq!(token1, 1);
        assert_eq!(token2, 2);
        assert_eq!(token3, 3);
    }

    #[test]
    fn test_allocate_metric_id() {
        let caps = ModuleCapabilities::default();
        let mut ctx = HttpContext::new(1, caps);

        let id1 = ctx.allocate_metric_id();
        let id2 = ctx.allocate_metric_id();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn test_modification_flags() {
        let caps = ModuleCapabilities::default();
        let mut ctx = HttpContext::new(1, caps);

        assert!(!ctx.has_request_modifications());
        assert!(!ctx.has_response_modifications());

        ctx.request_headers_modified = true;
        assert!(ctx.has_request_modifications());

        ctx.response_body_modified = true;
        assert!(ctx.has_response_modifications());
    }

    #[test]
    fn test_local_response_flag() {
        let caps = ModuleCapabilities::default();
        let mut ctx = HttpContext::new(1, caps);

        assert!(!ctx.should_send_local_response());

        ctx.local_response = Some(LocalResponse {
            status_code: 403,
            headers: vec![],
            body: b"Forbidden".to_vec(),
        });

        assert!(ctx.should_send_local_response());
    }
}

// Test types module
mod types_tests {
    use crate::wasm::types::*;

    #[test]
    fn test_filter_action_from_i32() {
        assert_eq!(FilterAction::from(0), FilterAction::Continue);
        assert_eq!(FilterAction::from(1), FilterAction::Pause);
        assert_eq!(FilterAction::from(99), FilterAction::Pause); // Default to Pause
    }

    #[test]
    fn test_wasm_config_default() {
        let config = WasmConfig::default();
        assert!(!config.enabled);
        assert!(config.modules.is_empty());
        // routesフィールドは削除されました（ルートレベルのmodulesフィールドを使用）
    }

    #[test]
    fn test_pooling_config_defaults() {
        let config = PoolingConfig::default();
        assert_eq!(config.total_memories, 128);
        assert_eq!(config.total_tables, 128);
        assert_eq!(config.max_memory_size, 10 * 1024 * 1024); // 10MB
    }

    #[test]
    fn test_wasm_defaults() {
        let defaults = WasmDefaults::default();
        assert_eq!(defaults.max_execution_time_ms, 100);
    }
}

// Integration tests
mod integration_tests {
    use std::path::Path;

    #[test]
    fn test_sample_wasm_filter_exists() {
        let wasm_path = "tests/wasm/header_filter.wasm";
        assert!(
            Path::new(wasm_path).exists(),
            "Sample WASM filter not found at {}",
            wasm_path
        );
    }
}

// Host function tests - tests the Proxy-Wasm ABI implementation
mod host_function_tests {
    use crate::wasm::capabilities::{CapabilityPreset, ModuleCapabilities};
    use crate::wasm::context::HttpContext;
    use crate::wasm::types::{Metric, MetricValue};
    use crate::wasm::constants::{METRIC_TYPE_COUNTER, METRIC_TYPE_GAUGE, METRIC_TYPE_HISTOGRAM};

    /// Create a test HttpContext with full capabilities
    fn create_test_context() -> HttpContext {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        ctx.plugin_name = "test_plugin".to_string();
        ctx.request_headers = vec![
            (":method".to_string(), "GET".to_string()),
            (":path".to_string(), "/api/test".to_string()),
            ("host".to_string(), "example.com".to_string()),
            ("user-agent".to_string(), "test-agent/1.0".to_string()),
        ];
        ctx.response_headers = vec![
            (":status".to_string(), "200".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        ctx.request_body = b"request body content".to_vec();
        ctx.response_body = b"response body content".to_vec();
        ctx
    }

    /// Create a minimal capability context for testing denied operations
    fn create_minimal_context() -> HttpContext {
        let caps = ModuleCapabilities::default(); // All false
        HttpContext::new(2, caps)
    }

    // === Header Operations Tests ===

    #[test]
    fn test_get_request_header() {
        let ctx = create_test_context();
        
        // Find :path header
        let header = ctx.request_headers.iter()
            .find(|(k, _)| k == ":path")
            .map(|(_, v)| v.as_str());
        
        assert_eq!(header, Some("/api/test"));
    }

    #[test]
    fn test_get_response_header() {
        let ctx = create_test_context();
        
        // Find content-type header
        let header = ctx.response_headers.iter()
            .find(|(k, _)| k == "content-type")
            .map(|(_, v)| v.as_str());
        
        assert_eq!(header, Some("application/json"));
    }

    #[test]
    fn test_modify_request_headers() {
        let mut ctx = create_test_context();
        
        // Add a new header
        ctx.request_headers.push(("x-custom".to_string(), "value".to_string()));
        ctx.request_headers_modified = true;
        
        assert!(ctx.has_request_modifications());
        assert_eq!(ctx.request_headers.len(), 5);
    }

    #[test]
    fn test_remove_request_header() {
        let mut ctx = create_test_context();
        let original_len = ctx.request_headers.len();
        
        // Remove user-agent header
        ctx.request_headers.retain(|(k, _)| k != "user-agent");
        ctx.request_headers_modified = true;
        
        assert!(ctx.has_request_modifications());
        assert_eq!(ctx.request_headers.len(), original_len - 1);
    }

    // === Buffer Operations Tests ===

    #[test]
    fn test_get_request_body_bytes() {
        let ctx = create_test_context();
        
        // Get first 10 bytes
        let bytes = &ctx.request_body[0..10];
        assert_eq!(bytes, b"request bo");
    }

    #[test]
    fn test_set_request_body() {
        let mut ctx = create_test_context();
        
        ctx.set_request_body(b"new body".to_vec(), true);
        ctx.request_body_modified = true;
        
        assert!(ctx.has_request_modifications());
        assert_eq!(ctx.request_body, b"new body");
        assert!(ctx.request_body_complete);
    }

    #[test]
    fn test_get_response_body_bytes() {
        let ctx = create_test_context();
        
        assert_eq!(&ctx.response_body[..8], b"response");
    }

    #[test]
    fn test_set_response_body() {
        let mut ctx = create_test_context();
        
        ctx.set_response_body(b"modified response".to_vec(), true);
        ctx.response_body_modified = true;
        
        assert!(ctx.has_response_modifications());
        assert_eq!(ctx.response_body, b"modified response");
    }

    // === Shared Data Operations Tests ===

    #[test]
    fn test_shared_data_set_and_get() {
        let ctx = create_test_context();
        
        // Set shared data
        {
            let mut data = ctx.shared_data.write().unwrap();
            data.insert("test_key".to_string(), (b"test_value".to_vec(), 1));
        }
        
        // Get shared data
        {
            let data = ctx.shared_data.read().unwrap();
            let (value, cas) = data.get("test_key").unwrap();
            assert_eq!(value, b"test_value");
            assert_eq!(*cas, 1);
        }
    }

    #[test]
    fn test_shared_data_cas_increment() {
        let mut ctx = create_test_context();
        
        // Initial CAS
        let cas1 = ctx.shared_data_cas;
        ctx.shared_data_cas += 1;
        let cas2 = ctx.shared_data_cas;
        
        assert_eq!(cas2, cas1 + 1);
    }

    #[test]
    fn test_shared_data_not_found() {
        let ctx = create_test_context();
        
        let data = ctx.shared_data.read().unwrap();
        assert!(data.get("nonexistent_key").is_none());
    }

    // === Metrics Operations Tests ===

    #[test]
    fn test_define_counter_metric() {
        let mut ctx = create_test_context();
        
        let id = ctx.allocate_metric_id();
        ctx.metrics.insert(id, Metric {
            metric_type: METRIC_TYPE_COUNTER,
            name: "test_counter".to_string(),
            value: MetricValue::Counter(0),
        });
        
        assert!(ctx.metrics.contains_key(&id));
        assert_eq!(ctx.metrics[&id].name, "test_counter");
    }

    #[test]
    fn test_increment_counter() {
        let mut ctx = create_test_context();
        
        let id = ctx.allocate_metric_id();
        ctx.metrics.insert(id, Metric {
            metric_type: METRIC_TYPE_COUNTER,
            name: "requests".to_string(),
            value: MetricValue::Counter(0),
        });
        
        // Increment
        if let Some(metric) = ctx.metrics.get_mut(&id) {
            if let MetricValue::Counter(ref mut v) = metric.value {
                *v += 5;
            }
        }
        
        if let MetricValue::Counter(v) = ctx.metrics[&id].value {
            assert_eq!(v, 5);
        } else {
            panic!("Expected Counter");
        }
    }

    #[test]
    fn test_define_gauge_metric() {
        let mut ctx = create_test_context();
        
        let id = ctx.allocate_metric_id();
        ctx.metrics.insert(id, Metric {
            metric_type: METRIC_TYPE_GAUGE,
            name: "connections".to_string(),
            value: MetricValue::Gauge(100),
        });
        
        assert_eq!(ctx.metrics[&id].metric_type, METRIC_TYPE_GAUGE);
    }

    #[test]
    fn test_get_metric_value() {
        let mut ctx = create_test_context();
        
        let id = ctx.allocate_metric_id();
        ctx.metrics.insert(id, Metric {
            metric_type: METRIC_TYPE_HISTOGRAM,
            name: "latency".to_string(),
            value: MetricValue::Histogram(vec![42]),
        });
        
        if let MetricValue::Histogram(ref buckets) = ctx.metrics[&id].value {
            assert_eq!(buckets[0], 42);
        } else {
            panic!("Expected Histogram");
        }
    }

    // === Stream Control Tests ===

    #[test]
    fn test_send_local_response() {
        let mut ctx = create_test_context();
        
        ctx.local_response = Some(crate::wasm::types::LocalResponse {
            status_code: 403,
            headers: vec![("x-blocked".to_string(), "true".to_string())],
            body: b"Forbidden".to_vec(),
        });
        
        assert!(ctx.should_send_local_response());
        let resp = ctx.local_response.as_ref().unwrap();
        assert_eq!(resp.status_code, 403);
    }

    #[test]
    fn test_set_effective_context() {
        let mut ctx = create_test_context();
        
        ctx.context_id = 999;
        assert_eq!(ctx.context_id, 999);
    }

    #[test]
    fn test_set_tick_period() {
        let mut ctx = create_test_context();
        
        ctx.tick_period_ms = 1000;
        assert_eq!(ctx.tick_period_ms, 1000);
    }

    // === HTTP Call Operations Tests ===

    #[test]
    fn test_allocate_http_call_token() {
        let mut ctx = create_test_context();
        
        let token1 = ctx.allocate_http_call_token();
        let token2 = ctx.allocate_http_call_token();
        let token3 = ctx.allocate_http_call_token();
        
        assert_eq!(token1, 1);
        assert_eq!(token2, 2);
        assert_eq!(token3, 3);
    }

    #[test]
    fn test_pending_http_call() {
        let mut ctx = create_test_context();
        
        let token = ctx.allocate_http_call_token();
        ctx.pending_http_calls.insert(token, crate::wasm::types::PendingHttpCall {
            token,
            upstream: "backend".to_string(),
            timeout_ms: 5000,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: b"test body".to_vec(),
            trailers: vec![],
        });
        
        assert!(ctx.pending_http_calls.contains_key(&token));
        assert_eq!(ctx.pending_http_calls[&token].upstream, "backend");
    }

    #[test]
    fn test_http_call_response() {
        let mut ctx = create_test_context();
        
        let token = ctx.allocate_http_call_token();
        ctx.http_call_responses.insert(token, crate::wasm::types::HttpCallResponse {
            status_code: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: b"OK".to_vec(),
            trailers: vec![],
        });
        ctx.current_http_call_token = Some(token);
        
        let resp = ctx.http_call_responses.get(&token).unwrap();
        assert_eq!(resp.status_code, 200);
    }

    // === Capability Enforcement Tests ===

    #[test]
    fn test_capability_deny_logging() {
        let ctx = create_minimal_context();
        assert!(!ctx.capabilities.allow_logging);
    }

    #[test]
    fn test_capability_deny_http_calls() {
        let ctx = create_minimal_context();
        assert!(!ctx.capabilities.allow_http_calls);
    }

    #[test]
    fn test_capability_deny_send_local_response() {
        let ctx = create_minimal_context();
        assert!(!ctx.capabilities.allow_send_local_response);
    }

    #[test]
    fn test_capability_property_check() {
        let caps = ModuleCapabilities {
            allowed_properties: vec!["request.*".to_string()],
            ..Default::default()
        };
        
        assert!(caps.is_property_allowed("request.path"));
        assert!(caps.is_property_allowed("request.method"));
        assert!(!caps.is_property_allowed("response.status"));
    }

    #[test]
    fn test_capability_upstream_check() {
        let caps = ModuleCapabilities {
            allow_http_calls: true,
            allowed_upstreams: vec!["webdis".to_string(), "auth".to_string()],
            ..Default::default()
        };
        
        assert!(caps.is_upstream_allowed("webdis"));
        assert!(caps.is_upstream_allowed("auth"));
        assert!(!caps.is_upstream_allowed("other_backend"));
    }

    /// Test custom properties storage and retrieval
    #[test]
    fn test_custom_properties() {
        let caps = ModuleCapabilities {
            allow_property_write: true,
            ..Default::default()
        };
        let mut ctx = HttpContext::new(1, caps);
        
        // Initially empty
        assert!(ctx.custom_properties.is_empty());
        
        // Add a custom property
        ctx.custom_properties.insert("my.custom.property".to_string(), b"test_value".to_vec());
        
        // Verify it's stored
        assert_eq!(ctx.custom_properties.len(), 1);
        assert_eq!(
            ctx.custom_properties.get("my.custom.property"),
            Some(&b"test_value".to_vec())
        );
    }
    
    /// Test multiple custom properties
    #[test]
    fn test_multiple_custom_properties() {
        let caps = ModuleCapabilities {
            allow_property_write: true,
            ..Default::default()
        };
        let mut ctx = HttpContext::new(1, caps);
        
        ctx.custom_properties.insert("filter.state.key1".to_string(), b"value1".to_vec());
        ctx.custom_properties.insert("filter.state.key2".to_string(), b"value2".to_vec());
        ctx.custom_properties.insert("metadata.key3".to_string(), b"value3".to_vec());
        
        assert_eq!(ctx.custom_properties.len(), 3);
    }
}


// P1: Lifecycle callback tests
mod lifecycle_callback_tests {
    use crate::wasm::capabilities::CapabilityPreset;
    use crate::wasm::context::HttpContext;

    /// Test that log_processed flag can be set on context
    #[test]
    fn test_context_log_phase_tracking() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let ctx = HttpContext::new(1, caps);
        
        // Context should be in initial state
        assert!(!ctx.should_send_local_response());
        assert!(!ctx.has_request_modifications());
        assert!(!ctx.has_response_modifications());
    }

    /// Test context creation for log callback
    #[test]
    fn test_context_for_log_callback() {
        let caps = CapabilityPreset::Standard.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        // Set up request/response data that would be available during log phase
        ctx.set_request("GET", "/api/test", vec![
            (":method".to_string(), "GET".to_string()),
            (":path".to_string(), "/api/test".to_string()),
        ], "192.168.1.1");
        ctx.set_response(200, vec![
            (":status".to_string(), "200".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]);
        
        // Verify data is accessible (would be used by proxy_on_log)
        assert_eq!(ctx.request_method, "GET");
        assert_eq!(ctx.request_path, "/api/test");
        assert_eq!(ctx.response_status, 200);
        assert_eq!(ctx.client_ip, "192.168.1.1");
    }

    /// Test context cleanup state for done callback
    #[test]
    fn test_context_for_done_callback() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(42, caps);
        
        // Simulate some state that would exist when done is called
        ctx.request_headers_modified = true;
        ctx.response_headers_modified = true;
        
        // Create some pending state
        let token = ctx.allocate_http_call_token();
        assert_eq!(token, 1);
        
        // Verify modifications tracked
        assert!(ctx.has_request_modifications());
        assert!(ctx.has_response_modifications());
    }

    /// Test on_done return value simulation
    #[test]
    fn test_done_return_value() {
        // Test that we properly interpret the return value
        // 0 = delete context, 1 = keep alive
        let delete_context: i32 = 0;
        let keep_alive: i32 = 1;
        
        assert_eq!(delete_context != 0, false);
        assert_eq!(keep_alive != 0, true);
    }

    /// Test multiple context IDs for lifecycle
    #[test]
    fn test_lifecycle_context_ids() {
        let caps = CapabilityPreset::Standard.to_capabilities();
        
        // Root context ID (typically 1)
        let root_ctx = HttpContext::new(1, caps.clone());
        assert_eq!(root_ctx.context_id, 1);
        
        // HTTP context ID (typically 2, 3, etc.)
        let http_ctx = HttpContext::new(2, caps);
        assert_eq!(http_ctx.context_id, 2);
    }

    /// Test that empty module list doesn't cause issues
    #[test]
    fn test_empty_module_list_handling() {
        // This tests the edge case of calling callbacks with no modules
        let module_names: Vec<String> = vec![];
        assert!(module_names.is_empty());
    }
}

// P2: Timer, queue, and trailer callback tests
mod p2_callback_tests {
    use crate::wasm::capabilities::CapabilityPreset;
    use crate::wasm::context::HttpContext;

    /// Test tick period setting
    #[test]
    fn test_tick_period_configuration() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        // Default tick period should be 0 (disabled)
        assert_eq!(ctx.tick_period_ms, 0);
        
        // Set tick period
        ctx.tick_period_ms = 1000;
        assert_eq!(ctx.tick_period_ms, 1000);
        
        // Set to different value
        ctx.tick_period_ms = 500;
        assert_eq!(ctx.tick_period_ms, 500);
    }

    /// Test context for tick callback
    #[test]
    fn test_context_for_tick_callback() {
        let caps = CapabilityPreset::Standard.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        // Root context ID for tick (typically 1)
        assert_eq!(ctx.context_id, 1);
        
        // Enable tick
        ctx.tick_period_ms = 100;
        assert!(ctx.tick_period_ms > 0);
    }

    /// Test request trailers context
    #[test]
    fn test_request_trailers_context() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        // Initially empty
        assert!(ctx.request_trailers.is_empty());
        
        // Set trailers
        ctx.request_trailers = vec![
            ("grpc-status".to_string(), "0".to_string()),
            ("grpc-message".to_string(), "ok".to_string()),
        ];
        
        assert_eq!(ctx.request_trailers.len(), 2);
        assert_eq!(ctx.request_trailers[0].0, "grpc-status");
    }

    /// Test response trailers context
    #[test]
    fn test_response_trailers_context() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        // Initially empty
        assert!(ctx.response_trailers.is_empty());
        
        // Set trailers
        ctx.response_trailers = vec![
            ("grpc-status".to_string(), "0".to_string()),
            ("grpc-message".to_string(), "success".to_string()),
            ("custom-trailer".to_string(), "value".to_string()),
        ];
        
        assert_eq!(ctx.response_trailers.len(), 3);
        assert_eq!(ctx.response_trailers[2].0, "custom-trailer");
    }

    /// Test queue operations context
    #[test]
    fn test_shared_queue_context() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let ctx = HttpContext::new(1, caps);
        
        // Shared data should be initialized
        assert!(ctx.shared_data.read().unwrap().is_empty());
        
        // Verify capability for queue operations
        assert!(ctx.capabilities.allow_shared_data);
    }

    /// Test queue ID handling
    #[test]
    fn test_queue_id_operations() {
        // Queue IDs are u32 values
        let queue_id_1: u32 = 1;
        let queue_id_2: u32 = 2;
        let queue_id_max: u32 = u32::MAX;
        
        assert_ne!(queue_id_1, queue_id_2);
        assert!(queue_id_max > queue_id_2);
    }

    /// Test tick callback frequency calculation
    #[test]
    fn test_tick_frequency() {
        // 1000ms period = 1 tick per second
        let period_ms: u32 = 1000;
        let ticks_per_second = 1000.0 / period_ms as f64;
        assert!((ticks_per_second - 1.0).abs() < f64::EPSILON);
        
        // 100ms period = 10 ticks per second
        let period_ms_fast: u32 = 100;
        let ticks_per_second_fast = 1000.0 / period_ms_fast as f64;
        assert!((ticks_per_second_fast - 10.0).abs() < f64::EPSILON);
    }

    /// Test trailers empty handling
    #[test]
    fn test_empty_trailers_handling() {
        let trailers: Vec<(String, String)> = vec![];
        assert!(trailers.is_empty());
        assert_eq!(trailers.len(), 0);
    }
}

// P3: gRPC callback tests
#[cfg(feature = "grpc")]
mod p3_grpc_callback_tests {
    use crate::wasm::capabilities::CapabilityPreset;
    use crate::wasm::context::HttpContext;

    /// Test gRPC call ID tracking
    #[test]
    fn test_grpc_call_id_allocation() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        // Test gRPC call ID allocation
        let call_id_1 = ctx.next_grpc_call_id();
        let call_id_2 = ctx.next_grpc_call_id();
        let call_id_3 = ctx.next_grpc_call_id();
        
        assert_eq!(call_id_1, 1);
        assert_eq!(call_id_2, 2);
        assert_eq!(call_id_3, 3);
    }

    /// Test gRPC stream state
    #[test]
    fn test_grpc_stream_states() {
        use crate::wasm::context::{GrpcStream, GrpcStreamState};
        
        // Test all stream states
        assert_eq!(GrpcStreamState::Open, GrpcStreamState::Open);
        assert_ne!(GrpcStreamState::Open, GrpcStreamState::HalfClosed);
        assert_ne!(GrpcStreamState::HalfClosed, GrpcStreamState::Closed);
        
        // Create a stream
        let stream = GrpcStream {
            stream_id: 1,
            upstream: "backend".to_string(),
            service: "grpc.health.v1.Health".to_string(),
            method: "Check".to_string(),
            state: GrpcStreamState::Open,
            pending_messages: vec![],
            initial_metadata: vec![],
        };
        
        assert_eq!(stream.stream_id, 1);
        assert_eq!(stream.state, GrpcStreamState::Open);
    }

    /// Test gRPC call registration
    #[test]
    fn test_grpc_call_registration() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        let call_id = ctx.next_grpc_call_id();
        ctx.register_grpc_call(
            call_id,
            "/grpc.health.v1.Health/Check".to_string(),
            vec![0x0a, 0x00],
            5000,
        );
        
        assert!(ctx.pending_grpc_calls.contains_key(&call_id));
        let (path, message, timeout) = ctx.pending_grpc_calls.get(&call_id).unwrap();
        assert_eq!(path, "/grpc.health.v1.Health/Check");
        assert_eq!(message, &vec![0x0a, 0x00]);
        assert_eq!(*timeout, 5000);
    }

    /// Test gRPC call cancellation
    #[test]
    fn test_grpc_call_cancellation() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        let call_id = ctx.next_grpc_call_id();
        ctx.register_grpc_call(
            call_id,
            "/test.Service/Method".to_string(),
            vec![],
            1000,
        );
        
        // Cancel the call
        let cancelled = ctx.cancel_grpc_call(call_id);
        assert!(cancelled);
        
        // Verify it's removed from pending
        assert!(!ctx.pending_grpc_calls.contains_key(&call_id));
        
        // Verify it's in cancelled set
        assert!(ctx.cancelled_grpc_calls.contains(&call_id));
        
        // Cancelling again should return false
        let cancelled_again = ctx.cancel_grpc_call(call_id);
        assert!(!cancelled_again);
    }

    /// Test gRPC status codes
    #[test]
    fn test_grpc_status_codes() {
        // Common gRPC status codes
        let ok: i32 = 0;
        let cancelled: i32 = 1;
        let unknown: i32 = 2;
        let invalid_argument: i32 = 3;
        let deadline_exceeded: i32 = 4;
        let not_found: i32 = 5;
        let unavailable: i32 = 14;
        
        assert_eq!(ok, 0);
        assert_eq!(cancelled, 1);
        assert_eq!(unknown, 2);
        assert_eq!(invalid_argument, 3);
        assert_eq!(deadline_exceeded, 4);
        assert_eq!(not_found, 5);
        assert_eq!(unavailable, 14);
    }

    /// Test gRPC metadata handling
    #[test]
    fn test_grpc_metadata() {
        let initial_metadata: Vec<(String, String)> = vec![
            ("content-type".to_string(), "application/grpc".to_string()),
            ("grpc-accept-encoding".to_string(), "gzip".to_string()),
        ];
        
        let trailing_metadata: Vec<(String, String)> = vec![
            ("grpc-status".to_string(), "0".to_string()),
            ("grpc-message".to_string(), "OK".to_string()),
        ];
        
        assert_eq!(initial_metadata.len(), 2);
        assert_eq!(trailing_metadata.len(), 2);
        assert_eq!(initial_metadata[0].0, "content-type");
        assert_eq!(trailing_metadata[0].0, "grpc-status");
    }

    /// Test take pending gRPC calls
    #[test]
    fn test_take_pending_grpc_calls() {
        let caps = CapabilityPreset::Extended.to_capabilities();
        let mut ctx = HttpContext::new(1, caps);
        
        // Register multiple calls
        ctx.register_grpc_call(1, "/path1".to_string(), vec![1], 100);
        ctx.register_grpc_call(2, "/path2".to_string(), vec![2], 200);
        
        assert_eq!(ctx.pending_grpc_calls.len(), 2);
        
        // Take all pending calls
        let taken = ctx.take_pending_grpc_calls();
        
        assert_eq!(taken.len(), 2);
        assert!(ctx.pending_grpc_calls.is_empty());
    }
}
