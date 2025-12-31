//! gRPC Stream State Management
//!
//! Manages the state of gRPC streams for different RPC types:
//! - Unary: Single request, single response
//! - Client Streaming: Multiple requests, single response
//! - Server Streaming: Single request, multiple responses
//! - Bidirectional: Multiple requests and responses

use std::time::Instant;

/// gRPC RPC types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcStreamType {
    /// Single request, single response
    Unary,
    /// Multiple requests (client streaming), single response
    ClientStreaming,
    /// Single request, multiple responses (server streaming)
    ServerStreaming,
    /// Multiple requests and responses (bidirectional streaming)
    BidirectionalStreaming,
}

impl GrpcStreamType {
    /// Check if this type expects multiple client messages
    pub fn is_client_streaming(&self) -> bool {
        matches!(self, Self::ClientStreaming | Self::BidirectionalStreaming)
    }

    /// Check if this type expects multiple server messages
    pub fn is_server_streaming(&self) -> bool {
        matches!(self, Self::ServerStreaming | Self::BidirectionalStreaming)
    }
}

impl Default for GrpcStreamType {
    fn default() -> Self {
        Self::Unary
    }
}

/// gRPC stream state
#[derive(Debug, Clone)]
pub struct GrpcStreamState {
    /// HTTP/2 stream ID
    pub stream_id: u32,
    /// RPC type (determined from request/response patterns)
    pub stream_type: GrpcStreamType,
    /// Whether client has finished sending (END_STREAM received)
    pub client_done: bool,
    /// Whether server has started sending response
    pub response_started: bool,
    /// Whether server has finished sending (END_STREAM sent)
    pub server_done: bool,
    /// Whether trailers have been sent
    pub trailers_sent: bool,
    /// Number of request frames received
    pub request_frames: usize,
    /// Number of response frames sent
    pub response_frames: usize,
    /// Stream creation time
    pub created_at: Instant,
    /// Last activity time
    pub last_activity: Instant,
}

impl GrpcStreamState {
    /// Create new stream state
    pub fn new(stream_id: u32) -> Self {
        let now = Instant::now();
        Self {
            stream_id,
            stream_type: GrpcStreamType::Unary,
            client_done: false,
            response_started: false,
            server_done: false,
            trailers_sent: false,
            request_frames: 0,
            response_frames: 0,
            created_at: now,
            last_activity: now,
        }
    }

    /// Record receipt of a request frame
    pub fn on_request_frame(&mut self) {
        self.request_frames += 1;
        self.last_activity = Instant::now();

        // If we receive multiple request frames, it's client streaming
        if self.request_frames > 1 {
            match self.stream_type {
                GrpcStreamType::Unary => {
                    self.stream_type = GrpcStreamType::ClientStreaming;
                }
                GrpcStreamType::ServerStreaming => {
                    self.stream_type = GrpcStreamType::BidirectionalStreaming;
                }
                _ => {}
            }
        }
    }

    /// Record client has finished sending (END_STREAM)
    pub fn on_client_done(&mut self) {
        self.client_done = true;
        self.last_activity = Instant::now();
    }

    /// Record sending of a response frame
    pub fn on_response_frame(&mut self) {
        self.response_frames += 1;
        self.response_started = true;
        self.last_activity = Instant::now();

        // If we send multiple response frames, it's server streaming
        if self.response_frames > 1 {
            match self.stream_type {
                GrpcStreamType::Unary => {
                    self.stream_type = GrpcStreamType::ServerStreaming;
                }
                GrpcStreamType::ClientStreaming => {
                    self.stream_type = GrpcStreamType::BidirectionalStreaming;
                }
                _ => {}
            }
        }
    }

    /// Record server has finished sending (END_STREAM)
    pub fn on_server_done(&mut self) {
        self.server_done = true;
        self.last_activity = Instant::now();
    }

    /// Record trailers sent
    pub fn on_trailers_sent(&mut self) {
        self.trailers_sent = true;
        self.server_done = true;
        self.last_activity = Instant::now();
    }

    /// Check if stream expects more client data
    pub fn expects_client_data(&self) -> bool {
        !self.client_done && self.stream_type.is_client_streaming()
    }

    /// Check if stream expects more server data
    pub fn expects_server_data(&self) -> bool {
        !self.server_done && self.stream_type.is_server_streaming()
    }

    /// Check if stream is complete (both sides done)
    pub fn is_complete(&self) -> bool {
        self.client_done && self.server_done
    }

    /// Check if stream is ready to send trailers
    pub fn can_send_trailers(&self) -> bool {
        !self.trailers_sent && self.response_started
    }

    /// Get stream age
    pub fn age(&self) -> std::time::Duration {
        self.created_at.elapsed()
    }

    /// Get time since last activity
    pub fn idle_time(&self) -> std::time::Duration {
        self.last_activity.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unary_stream() {
        let mut state = GrpcStreamState::new(1);

        assert_eq!(state.stream_type, GrpcStreamType::Unary);

        // Single request
        state.on_request_frame();
        state.on_client_done();

        assert_eq!(state.stream_type, GrpcStreamType::Unary);
        assert!(state.client_done);

        // Single response
        state.on_response_frame();
        state.on_trailers_sent();

        assert!(state.is_complete());
        assert_eq!(state.request_frames, 1);
        assert_eq!(state.response_frames, 1);
    }

    #[test]
    fn test_client_streaming() {
        let mut state = GrpcStreamState::new(1);

        // Multiple requests
        state.on_request_frame();
        state.on_request_frame();
        state.on_request_frame();
        state.on_client_done();

        assert_eq!(state.stream_type, GrpcStreamType::ClientStreaming);
        assert_eq!(state.request_frames, 3);
    }

    #[test]
    fn test_server_streaming() {
        let mut state = GrpcStreamState::new(1);

        // Single request
        state.on_request_frame();
        state.on_client_done();

        // Multiple responses
        state.on_response_frame();
        state.on_response_frame();
        state.on_response_frame();

        assert_eq!(state.stream_type, GrpcStreamType::ServerStreaming);
        assert_eq!(state.response_frames, 3);
    }

    #[test]
    fn test_bidirectional_streaming() {
        let mut state = GrpcStreamState::new(1);

        // Multiple requests and responses
        state.on_request_frame();
        state.on_response_frame();
        state.on_request_frame();
        state.on_response_frame();

        assert_eq!(state.stream_type, GrpcStreamType::BidirectionalStreaming);
    }

    #[test]
    fn test_stream_type_methods() {
        assert!(!GrpcStreamType::Unary.is_client_streaming());
        assert!(!GrpcStreamType::Unary.is_server_streaming());

        assert!(GrpcStreamType::ClientStreaming.is_client_streaming());
        assert!(!GrpcStreamType::ClientStreaming.is_server_streaming());

        assert!(!GrpcStreamType::ServerStreaming.is_client_streaming());
        assert!(GrpcStreamType::ServerStreaming.is_server_streaming());

        assert!(GrpcStreamType::BidirectionalStreaming.is_client_streaming());
        assert!(GrpcStreamType::BidirectionalStreaming.is_server_streaming());
    }
}
