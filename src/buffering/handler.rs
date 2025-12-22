//! バッファリングハンドラー
//!
//! バックエンドからのレスポンスをバッファリングし、
//! バックエンド接続を早期に解放する機能を提供します。

use super::config::BufferingConfig;
use std::io;
use std::path::PathBuf;

/// バッファされたレスポンス
#[derive(Debug)]
pub struct BufferedResponse {
    /// レスポンスヘッダー（生データ）
    pub headers: Vec<u8>,
    /// レスポンスボディ
    pub body: BufferedBody,
    /// HTTPステータスコード
    pub status_code: u16,
    /// バックエンドがKeep-Aliveを希望しているか
    pub backend_keep_alive: bool,
}

/// バッファされたボディ
#[derive(Debug)]
pub enum BufferedBody {
    /// メモリ内バッファ
    Memory(Vec<u8>),
    /// ディスクバッファ（大きいレスポンス用）
    Disk {
        path: PathBuf,
        size: u64,
    },
    /// ストリーミング（バッファリング不使用）
    Streaming,
}

impl BufferedBody {
    /// ボディサイズを取得
    pub fn size(&self) -> u64 {
        match self {
            BufferedBody::Memory(data) => data.len() as u64,
            BufferedBody::Disk { size, .. } => *size,
            BufferedBody::Streaming => 0,
        }
    }
    
    /// メモリ内データを取得（ディスクの場合はNone）
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            BufferedBody::Memory(data) => Some(data),
            _ => None,
        }
    }
    
    /// メモリ内データを消費して取得
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            BufferedBody::Memory(data) => Some(data),
            _ => None,
        }
    }
}

/// バッファリングハンドラー
/// 
/// スレッドローカルで使用され、バッファリング操作を管理します。
pub struct BufferingHandler {
    config: BufferingConfig,
    /// 現在のメモリ使用量（概算）
    memory_usage: usize,
}

impl BufferingHandler {
    /// 新しいハンドラーを作成
    pub fn new(config: BufferingConfig) -> Self {
        Self {
            config,
            memory_usage: 0,
        }
    }
    
    /// 設定を取得
    #[inline]
    pub fn config(&self) -> &BufferingConfig {
        &self.config
    }
    
    /// バッファリングが必要かどうかを判定
    #[inline]
    pub fn should_buffer(&self, content_length: Option<usize>) -> bool {
        self.config.should_buffer(content_length)
    }
    
    /// メモリバッファを作成
    /// 
    /// 指定されたサイズに基づいて適切な初期容量でバッファを作成します。
    pub fn create_buffer(&mut self, expected_size: Option<usize>) -> Vec<u8> {
        let capacity = expected_size
            .map(|s| s.min(self.config.max_memory_buffer))
            .unwrap_or(64 * 1024); // デフォルト64KB
        
        self.memory_usage += capacity;
        Vec::with_capacity(capacity)
    }
    
    /// バッファにデータを追加
    /// 
    /// メモリ制限を超えた場合はエラーを返します。
    /// 将来的にはディスクへのスピルオーバーを実装予定。
    pub fn append_to_buffer(&mut self, buffer: &mut Vec<u8>, data: &[u8]) -> io::Result<()> {
        let new_size = buffer.len() + data.len();
        
        if new_size > self.config.max_memory_buffer {
            // メモリ制限超過
            if self.config.disk_buffer_available() {
                // TODO: ディスクバッファへのスピルオーバー
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "memory buffer exceeded, disk spillover not yet implemented"
                ));
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!(
                        "response size {} exceeds max_memory_buffer {}",
                        new_size, self.config.max_memory_buffer
                    )
                ));
            }
        }
        
        buffer.extend_from_slice(data);
        self.memory_usage = self.memory_usage.saturating_add(data.len());
        Ok(())
    }
    
    /// バッファを完了してBufferedResponseを作成
    pub fn finalize_buffer(
        &mut self,
        headers: Vec<u8>,
        body: Vec<u8>,
        status_code: u16,
        backend_keep_alive: bool,
    ) -> BufferedResponse {
        // メモリ使用量を調整
        self.memory_usage = self.memory_usage.saturating_sub(body.capacity());
        
        BufferedResponse {
            headers,
            body: BufferedBody::Memory(body),
            status_code,
            backend_keep_alive,
        }
    }
    
    /// ストリーミングレスポンスを作成（バッファリングなし）
    pub fn streaming_response(
        &self,
        status_code: u16,
        backend_keep_alive: bool,
    ) -> BufferedResponse {
        BufferedResponse {
            headers: Vec::new(),
            body: BufferedBody::Streaming,
            status_code,
            backend_keep_alive,
        }
    }
    
    /// 現在のメモリ使用量を取得
    #[inline]
    pub fn memory_usage(&self) -> usize {
        self.memory_usage
    }
    
    /// ハンドラーをリセット
    pub fn reset(&mut self) {
        self.memory_usage = 0;
    }
}

/// ディスクバッファ操作（monoio::fs使用）
#[cfg(target_os = "linux")]
pub mod disk_buffer {
    use monoio::fs::File;
    use std::io;
    use std::path::Path;
    use xxhash_rust::xxh3::xxh3_64;
    
    /// ディスクバッファへの非同期書き込み
    pub async fn write_to_disk(base_path: &Path, key: &[u8], data: Vec<u8>) -> io::Result<std::path::PathBuf> {
        let hash = xxh3_64(key);
        
        // ハッシュベースのパス生成（ディレクトリ分散）
        let dir1 = format!("{:02x}", (hash >> 56) as u8);
        let dir2 = format!("{:02x}", (hash >> 48) as u8);
        let filename = format!("{:016x}.buf", hash);
        
        let dir_path = base_path.join(&dir1).join(&dir2);
        let file_path = dir_path.join(&filename);
        
        // ディレクトリ作成（同期だが頻度は低い）
        std::fs::create_dir_all(&dir_path)?;
        
        // io_uring による非同期書き込み
        let file = File::create(&file_path).await?;
        let (res, _) = file.write_all_at(data, 0).await;
        res?;
        
        // fsync（データ整合性のため）
        file.sync_all().await?;
        
        Ok(file_path)
    }
    
    /// ディスクバッファからの非同期読み込み
    pub async fn read_from_disk(path: &Path) -> io::Result<Vec<u8>> {
        let file = File::open(path).await?;
        
        // ファイルサイズ取得
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len() as usize;
        
        let mut buf = Vec::with_capacity(size);
        #[allow(clippy::uninit_vec)]
        unsafe { buf.set_len(size); }
        
        // io_uring による非同期読み込み
        let (res, buf) = file.read_exact_at(buf, 0).await;
        res?;
        
        Ok(buf)
    }
    
    /// ディスクバッファを削除
    pub fn remove_disk_buffer(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffering::BufferingMode;

    #[test]
    fn test_handler_create_buffer() {
        let config = BufferingConfig {
            mode: BufferingMode::Full,
            max_memory_buffer: 1024 * 1024,
            ..Default::default()
        };
        let mut handler = BufferingHandler::new(config);
        
        let buf = handler.create_buffer(Some(1024));
        assert_eq!(buf.capacity(), 1024);
        assert_eq!(handler.memory_usage(), 1024);
    }

    #[test]
    fn test_handler_append_within_limit() {
        let config = BufferingConfig {
            mode: BufferingMode::Full,
            max_memory_buffer: 1024,
            ..Default::default()
        };
        let mut handler = BufferingHandler::new(config);
        let mut buf = Vec::new();
        
        let result = handler.append_to_buffer(&mut buf, &[1, 2, 3, 4]);
        assert!(result.is_ok());
        assert_eq!(buf, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_handler_append_exceeds_limit() {
        let config = BufferingConfig {
            mode: BufferingMode::Full,
            max_memory_buffer: 10,
            disk_buffer_path: None,
            ..Default::default()
        };
        let mut handler = BufferingHandler::new(config);
        let mut buf = Vec::new();
        
        // 10バイトまではOK
        let result = handler.append_to_buffer(&mut buf, &[0; 10]);
        assert!(result.is_ok());
        
        // 11バイト目でエラー
        let result = handler.append_to_buffer(&mut buf, &[0; 1]);
        assert!(result.is_err());
    }

    #[test]
    fn test_buffered_body_size() {
        let body = BufferedBody::Memory(vec![1, 2, 3, 4, 5]);
        assert_eq!(body.size(), 5);
        
        let body = BufferedBody::Disk {
            path: PathBuf::from("/tmp/test"),
            size: 1000,
        };
        assert_eq!(body.size(), 1000);
        
        let body = BufferedBody::Streaming;
        assert_eq!(body.size(), 0);
    }
}

