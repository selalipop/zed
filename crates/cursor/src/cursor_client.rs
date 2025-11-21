use anyhow::{Result, anyhow};
use flate2::write::GzEncoder;
use flate2::read::GzDecoder;
use flate2::Compression;
use futures::AsyncReadExt;
use http_client::{AsyncBody, HttpClient, Method};
use prost::Message;
use std::io::{Write, Read};
use std::sync::Arc;
use uuid::Uuid;
use sha2::{Sha256, Digest};
use chrono::Local;

mod unite {
    include!(concat!(env!("OUT_DIR"), "/unite.rs"));
}

pub use unite::{
    StreamCppRequest, StreamCppResponse, CurrentFileInfo, CursorPosition, CursorRange,
    AdditionalFile, CppContextItem, CppFate, RecordCppFateRequest, RecordCppFateResponse,
    RefreshTabContextRequest, RefreshTabContextResponse, FsSyncFileRequest, FsSyncFileResponse,
    CodeResult, RepositoryInfo, FilesyncUpdateWithModelVersion, LineRange,
};

const STREAM_CPP_ENDPOINT: &str = "https://api2.cursor.sh/aiserver.v1.AiService/StreamCpp";
const RECORD_FATE_ENDPOINT: &str = "https://api2.cursor.sh/aiserver.v1.AiService/RecordCppFate";
const REFRESH_CONTEXT_ENDPOINT: &str = "https://api2.cursor.sh/aiserver.v1.AiService/RefreshTabContext";
const FS_SYNC_ENDPOINT: &str = "https://api2.cursor.sh/aiserver.v1.AiService/FSSyncFile";

pub struct CursorClient {
    http_client: Arc<dyn HttpClient>,
    pub auth_token: Option<String>,
    pub session_id: String,
    pub client_key: String,
    pub fs_client_key: String,
    pub workspace_id: String,
}

impl CursorClient {
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        let auth_token = std::env::var("CURSOR_AUTH_TOKEN").ok();
        log::info!("CursorClient::new() - auth_token present: {}", auth_token.is_some());
        
        let session_id = Uuid::new_v4().to_string();
        let client_key = Self::generate_client_key();
        let fs_client_key = Self::generate_fs_client_key();
        let workspace_id = Uuid::new_v4().to_string();
        
        log::info!("CursorClient::new() - session_id: {}, workspace_id: {}", session_id, workspace_id);
        
        Self {
            http_client,
            auth_token,
            session_id,
            client_key,
            fs_client_key,
            workspace_id,
        }
    }
    
    fn generate_client_key() -> String {
        let random_data = Uuid::new_v4().to_string();
        let mut hasher = Sha256::new();
        hasher.update(random_data.as_bytes());
        format!("{:x}", hasher.finalize())
    }
    
    fn generate_fs_client_key() -> String {
        let random_data = Uuid::new_v4().to_string();
        let mut hasher = Sha256::new();
        hasher.update(b"fs-");
        hasher.update(random_data.as_bytes());
        format!("{:x}", hasher.finalize())
    }
    
    fn compute_checksum(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = hasher.finalize();
        format!("TPn75BGq{:x}", hash)
    }
    
    fn get_timezone() -> String {
        Local::now().format("%Z").to_string()
    }

    pub fn http_client(&self) -> Arc<dyn HttpClient> {
        self.http_client.clone()
    }

    pub async fn get_completion(
        http_client: Arc<dyn HttpClient>, 
        auth_token: Option<String>,
        session_id: String,
        client_key: String,
        fs_client_key: String,
        request_id: String,
        request: StreamCppRequest
    ) -> Result<(String, Option<String>, Option<LineRange>)> {
        log::info!("get_completion: Encoding request protobuf");
        let serialized = request.encode_to_vec();
        log::info!("get_completion: Serialized {} bytes, compressing", serialized.len());
        let compressed = Self::compress_protobuf(&serialized)?;
        log::info!("get_completion: Compressed to {} bytes", compressed.len());
        
        let flags: u8 = 0x01;
        let length = (compressed.len() as u32).to_be_bytes();
        let mut envelope = Vec::with_capacity(1 + 4 + compressed.len());
        envelope.push(flags);
        envelope.extend_from_slice(&length);
        envelope.extend_from_slice(&compressed);

        let config_version = Uuid::new_v4().to_string();
        let checksum = Self::compute_checksum(&envelope);
        let timezone = Self::get_timezone();

        let mut request_builder = http_client::Request::builder()
            .method(Method::POST)
            .uri(STREAM_CPP_ENDPOINT)
            .header("Content-Type", "application/connect+proto")
            .header("Connect-Protocol-Version", "1")
            .header("Connect-Accept-Encoding", "gzip")
            .header("Connect-Content-Encoding", "gzip")
            .header("User-Agent", "connect-es/1.6.1")
            .header("X-Cursor-Client-Type", "ide")
            .header("X-Cursor-Client-Version", "2.1.17")
            .header("X-Cursor-Streaming", "true")
            .header("X-Ghost-Mode", "true")
            .header("X-Session-Id", &session_id)
            .header("X-Request-Id", &request_id)
            .header("X-Cursor-Config-Version", &config_version)
            .header("X-Cursor-Timezone", &timezone)
            .header("X-Client-Key", &client_key)
            .header("X-Fs-Client-Key", &fs_client_key)
            .header("X-Cursor-Checksum", &checksum);
        
        if let Some(token) = &auth_token {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", token));
        } else {
            log::warn!("No auth token provided - request may fail");
        }

        let http_request = request_builder.body(AsyncBody::from(envelope))?;

        log::info!("get_completion: Sending HTTP request to {}", STREAM_CPP_ENDPOINT);
        let mut response = http_client.send(http_request).await?;
        log::info!("get_completion: Received HTTP response with status: {}", response.status());
        
        if !response.status().is_success() {
            let mut error_body = String::new();
            let _ = response.body_mut().read_to_string(&mut error_body).await;
            log::error!("get_completion: API returned error status {}: {}", response.status(), error_body);
            return Err(anyhow!("API returned status {}: {}", response.status(), error_body));
        }

        log::info!("get_completion: Reading response body");
        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;
        log::info!("get_completion: Received {} bytes in response body", body_bytes.len());
        
        log::info!("get_completion: Parsing streaming response");
        let (completion_text, binding_id, range_to_replace) = Self::parse_streaming_response(&body_bytes)?;
        log::info!("get_completion: Parsed completion text: {} chars, binding_id: {:?}, range: {:?}", completion_text.len(), binding_id, range_to_replace);
        
        Ok((completion_text, binding_id, range_to_replace))
    }

    fn compress_protobuf(data: &[u8]) -> Result<Vec<u8>> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data)?;
        Ok(encoder.finish()?)
    }

    fn decompress_protobuf(data: &[u8]) -> Result<Vec<u8>> {
        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)?;
        Ok(decompressed)
    }

    fn parse_streaming_response(data: &[u8]) -> Result<(String, Option<String>, Option<LineRange>)> {
        let mut buffer = data;
        let mut completion_text = String::new();
        let mut binding_id: Option<String> = None;
        let mut range_to_replace: Option<LineRange> = None;

        while buffer.len() >= 5 {
            let flags = buffer[0];
            let msg_length = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
            
            if buffer.len() < 5 + msg_length {
                break;
            }
            
            let msg_data = &buffer[5..5 + msg_length];
            buffer = &buffer[5 + msg_length..];
            
            let is_compressed = (flags & 0x01) != 0;
            let is_end_marker = (flags & 0x02) != 0;
            
            log::info!("parse_streaming_response: Message flags={:#04x}, length={}, compressed={}, end_marker={}", 
                       flags, msg_length, is_compressed, is_end_marker);
            
            let decompressed = if is_compressed {
                log::info!("parse_streaming_response: Decompressing message");
                Self::decompress_protobuf(msg_data)?
            } else {
                log::info!("parse_streaming_response: Message not compressed");
                msg_data.to_vec()
            };
            
            if let Ok(response) = StreamCppResponse::decode(&decompressed[..]) {
                log::info!("parse_streaming_response: Decoded response, text='{}', text_len={}, done_stream={:?}, binding_id={:?}, range_to_replace={:?}", 
                           response.text, response.text.len(), response.done_stream, response.binding_id, response.range_to_replace);
                completion_text.push_str(&response.text);
                
                if response.binding_id.is_some() {
                    binding_id = response.binding_id.clone();
                }
                
                if response.range_to_replace.is_some() && range_to_replace.is_none() {
                    range_to_replace = response.range_to_replace.clone();
                }
                
                if response.done_stream.unwrap_or(false) || is_end_marker {
                    log::info!("parse_streaming_response: End of stream detected");
                    break;
                }
            } else {
                log::warn!("parse_streaming_response: Failed to decode StreamCppResponse");
            }
        }

        Ok((completion_text, binding_id, range_to_replace))
    }

    pub async fn record_cpp_fate(
        http_client: Arc<dyn HttpClient>,
        auth_token: Option<String>,
        session_id: String,
        client_key: String,
        fs_client_key: String,
        request_id: String,
        fate: CppFate,
    ) -> Result<()> {
        let request = RecordCppFateRequest {
            request_id,
            performance_now_time: 0.0,
            fate: fate as i32,
            extension: "rs".to_string(),
        };

        let serialized = request.encode_to_vec();
        let compressed = Self::compress_protobuf(&serialized)?;

        let flags: u8 = 0x01;
        let length = (compressed.len() as u32).to_be_bytes();
        let mut envelope = Vec::with_capacity(1 + 4 + compressed.len());
        envelope.push(flags);
        envelope.extend_from_slice(&length);
        envelope.extend_from_slice(&compressed);

        let req_id = Uuid::new_v4().to_string();
        let config_version = Uuid::new_v4().to_string();
        let checksum = Self::compute_checksum(&envelope);
        let timezone = Self::get_timezone();

        let mut request_builder = http_client::Request::builder()
            .method(Method::POST)
            .uri(RECORD_FATE_ENDPOINT)
            .header("Content-Type", "application/connect+proto")
            .header("Connect-Protocol-Version", "1")
            .header("Connect-Accept-Encoding", "gzip")
            .header("Connect-Content-Encoding", "gzip")
            .header("User-Agent", "connect-es/1.6.1")
            .header("X-Cursor-Client-Type", "ide")
            .header("X-Cursor-Client-Version", "2.1.17")
            .header("X-Cursor-Streaming", "true")
            .header("X-Ghost-Mode", "true")
            .header("X-Session-Id", &session_id)
            .header("X-Request-Id", &req_id)
            .header("X-Cursor-Config-Version", &config_version)
            .header("X-Cursor-Timezone", &timezone)
            .header("X-Client-Key", &client_key)
            .header("X-Fs-Client-Key", &fs_client_key)
            .header("X-Cursor-Checksum", &checksum);

        if let Some(token) = &auth_token {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", token));
        }

        let http_request = request_builder.body(AsyncBody::from(envelope))?;
        let mut response = http_client.send(http_request).await?;

        if !response.status().is_success() {
            let mut error_body = String::new();
            let _ = response.body_mut().read_to_string(&mut error_body).await;
            return Err(anyhow!("RecordCppFate returned status {}: {}", response.status(), error_body));
        }

        Ok(())
    }

    pub async fn refresh_tab_context(
        http_client: Arc<dyn HttpClient>,
        auth_token: Option<String>,
        session_id: String,
        client_key: String,
        fs_client_key: String,
        request: RefreshTabContextRequest,
    ) -> Result<Vec<CodeResult>> {
        let serialized = request.encode_to_vec();
        let compressed = Self::compress_protobuf(&serialized)?;

        let flags: u8 = 0x01;
        let length = (compressed.len() as u32).to_be_bytes();
        let mut envelope = Vec::with_capacity(1 + 4 + compressed.len());
        envelope.push(flags);
        envelope.extend_from_slice(&length);
        envelope.extend_from_slice(&compressed);

        let request_id = Uuid::new_v4().to_string();
        let config_version = Uuid::new_v4().to_string();
        let checksum = Self::compute_checksum(&envelope);
        let timezone = Self::get_timezone();

        let mut request_builder = http_client::Request::builder()
            .method(Method::POST)
            .uri(REFRESH_CONTEXT_ENDPOINT)
            .header("Content-Type", "application/connect+proto")
            .header("Connect-Protocol-Version", "1")
            .header("Connect-Accept-Encoding", "gzip")
            .header("Connect-Content-Encoding", "gzip")
            .header("User-Agent", "connect-es/1.6.1")
            .header("X-Cursor-Client-Type", "ide")
            .header("X-Cursor-Client-Version", "2.1.17")
            .header("X-Cursor-Streaming", "true")
            .header("X-Ghost-Mode", "true")
            .header("X-Session-Id", &session_id)
            .header("X-Request-Id", &request_id)
            .header("X-Cursor-Config-Version", &config_version)
            .header("X-Cursor-Timezone", &timezone)
            .header("X-Client-Key", &client_key)
            .header("X-Fs-Client-Key", &fs_client_key)
            .header("X-Cursor-Checksum", &checksum);

        if let Some(token) = &auth_token {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", token));
        }

        let http_request = request_builder.body(AsyncBody::from(envelope))?;
        let mut response = http_client.send(http_request).await?;

        if !response.status().is_success() {
            let mut error_body = String::new();
            let _ = response.body_mut().read_to_string(&mut error_body).await;
            return Err(anyhow!("RefreshTabContext returned status {}: {}", response.status(), error_body));
        }

        let mut body_bytes = Vec::new();
        response.body_mut().read_to_end(&mut body_bytes).await?;

        if body_bytes.len() < 5 {
            return Ok(vec![]);
        }

        let flags = body_bytes[0];
        let msg_length = u32::from_be_bytes([body_bytes[1], body_bytes[2], body_bytes[3], body_bytes[4]]) as usize;

        if body_bytes.len() < 5 + msg_length {
            return Ok(vec![]);
        }

        let msg_data = &body_bytes[5..5 + msg_length];
        let is_compressed = (flags & 0x01) != 0;

        let decompressed = if is_compressed {
            Self::decompress_protobuf(msg_data)?
        } else {
            msg_data.to_vec()
        };

        if let Ok(response) = RefreshTabContextResponse::decode(&decompressed[..]) {
            Ok(response.code_results)
        } else {
            Ok(vec![])
        }
    }

    pub async fn fs_sync_file(
        http_client: Arc<dyn HttpClient>,
        auth_token: Option<String>,
        session_id: String,
        client_key: String,
        fs_client_key: String,
        request: FsSyncFileRequest,
    ) -> Result<()> {
        let serialized = request.encode_to_vec();
        let compressed = Self::compress_protobuf(&serialized)?;

        let flags: u8 = 0x01;
        let length = (compressed.len() as u32).to_be_bytes();
        let mut envelope = Vec::with_capacity(1 + 4 + compressed.len());
        envelope.push(flags);
        envelope.extend_from_slice(&length);
        envelope.extend_from_slice(&compressed);

        let request_id = Uuid::new_v4().to_string();
        let config_version = Uuid::new_v4().to_string();
        let checksum = Self::compute_checksum(&envelope);
        let timezone = Self::get_timezone();

        let mut request_builder = http_client::Request::builder()
            .method(Method::POST)
            .uri(FS_SYNC_ENDPOINT)
            .header("Content-Type", "application/connect+proto")
            .header("Connect-Protocol-Version", "1")
            .header("Connect-Accept-Encoding", "gzip")
            .header("Connect-Content-Encoding", "gzip")
            .header("User-Agent", "connect-es/1.6.1")
            .header("X-Cursor-Client-Type", "ide")
            .header("X-Cursor-Client-Version", "2.1.17")
            .header("X-Cursor-Streaming", "true")
            .header("X-Ghost-Mode", "true")
            .header("X-Session-Id", &session_id)
            .header("X-Request-Id", &request_id)
            .header("X-Cursor-Config-Version", &config_version)
            .header("X-Cursor-Timezone", &timezone)
            .header("X-Client-Key", &client_key)
            .header("X-Fs-Client-Key", &fs_client_key)
            .header("X-Cursor-Checksum", &checksum);

        if let Some(token) = &auth_token {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", token));
        }

        let http_request = request_builder.body(AsyncBody::from(envelope))?;
        let mut response = http_client.send(http_request).await?;

        if !response.status().is_success() {
            let mut error_body = String::new();
            let _ = response.body_mut().read_to_string(&mut error_body).await;
            return Err(anyhow!("FSSyncFile returned status {}: {}", response.status(), error_body));
        }

        Ok(())
    }
}
