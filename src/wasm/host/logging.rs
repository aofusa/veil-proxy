//! Logging Host Functions

use crate::wasm::exports::GuestMemory;
use ftlog::{debug, error, info, trace, warn};
use std::fmt::Write as _;
use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// ゲストが渡した文字列をアプリのログへ書く形（制御文字を `\n`・`\r`・`\t`・`\u{..}` で書く）。
// B-454: 以前はそのまま書き、プラグインがクライアントの値（WAF の復号した一致等）を書くと、改行で偽の行を足したり、
// 端末のエスケープシーケンスを埋めたりできた（アクセスログは write_text_escaped で同じことをしている）
pub(crate) struct LogSafe<'a>(pub(crate) &'a str);

impl std::fmt::Display for LogSafe<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.0.chars().any(char::is_control) {
            return f.write_str(self.0);
        }
        for c in self.0.chars() {
            match c {
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                c if c.is_control() => write!(f, "\\u{{{:x}}}", c as u32)?,
                c => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

/// Add logging functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_log
    linker.func_wrap(
        "env",
        "proxy_log",
        |mut caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_size: i32| -> i32 {
            // Read message from WASM memory first
            let memory = match caller.guest_memory() {
                Some(mem) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data(&caller);
            let start = msg_ptr as u32 as usize;
            let end = start + msg_size as u32 as usize;

            if end > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            let msg = match std::str::from_utf8(&data[start..end]) {
                Ok(s) => s,
                Err(_) => return PROXY_RESULT_PARSE_FAILURE,
            };

            // Check capability after reading message
            let state = caller.data();
            if !state.http_ctx.capabilities.allow_logging {
                return PROXY_RESULT_OK; // Silently ignore
            }

            let plugin_name = &state.http_ctx.plugin_name;
            let msg = LogSafe(msg);

            match level {
                LOG_TRACE => trace!("[wasm:{}] {}", plugin_name, msg),
                LOG_DEBUG => debug!("[wasm:{}] {}", plugin_name, msg),
                LOG_INFO => info!("[wasm:{}] {}", plugin_name, msg),
                LOG_WARN => warn!("[wasm:{}] {}", plugin_name, msg),
                LOG_ERROR => error!("[wasm:{}] {}", plugin_name, msg),
                LOG_CRITICAL => error!("[wasm:{}] CRITICAL: {}", plugin_name, msg),
                _ => info!("[wasm:{}] {}", plugin_name, msg),
            }

            PROXY_RESULT_OK
        },
    )?;

    // proxy_get_current_time_nanoseconds
    linker.func_wrap(
        "env",
        "proxy_get_current_time_nanoseconds",
        |mut caller: Caller<'_, HostState>, return_time_ptr: i32| -> i32 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);

            // Write to WASM memory
            let memory = match caller.guest_memory() {
                Some(mem) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data_mut(&mut caller);
            let ptr = return_time_ptr as u32 as usize;

            if ptr + 8 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            data[ptr..ptr + 8].copy_from_slice(&now.to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::LogSafe;

    /// B-454: 制御文字は見える形で書き、行を足せない（普通の文字・非 ASCII はそのまま）
    #[test]
    fn log_safe_escapes_control_characters() {
        assert_eq!(LogSafe("plain テキスト").to_string(), "plain テキスト");
        assert_eq!(
            LogSafe("a\nERROR forged\r\x1b[31mred\t\u{85}\x7f").to_string(),
            "a\\nERROR forged\\r\\u{1b}[31mred\\t\\u{85}\\u{7f}"
        );
    }
}
