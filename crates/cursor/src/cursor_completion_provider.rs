use crate::cursor_client::{CursorClient, StreamCppRequest, CurrentFileInfo, CursorPosition, CppFate, LineRange};
use anyhow::Result;
use clock::Global as ClockVersion;
use edit_prediction::{EditPrediction, EditPredictionProvider, Direction};
use gpui::{App, Context, Entity, EntityId, Task};
use language::{Anchor, Buffer, BufferSnapshot, Point};
use project::Project;
use std::{
    ops::{AddAssign, Range},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use text::{ToOffset, ToPoint};
use unicode_segmentation::UnicodeSegmentation;

const ENABLE_CACHING: bool = false;
const ENABLE_FATE_TRACKING: bool = false;


pub struct CursorCompletionProvider {
    cursor_client: Entity<CursorClient>,
    project: Entity<Project>,
    buffer_id: Option<EntityId>,
    completion_text: Option<String>,
    pending_refresh: Option<Task<Result<()>>>,
    completion_position: Option<Anchor>,
    current_binding_id: Option<String>,
    current_request_id: Option<String>,
    last_buffer_version: Option<ClockVersion>,
    range_to_replace: Option<LineRange>,
}

impl CursorCompletionProvider {
    pub fn new(cursor_client: Entity<CursorClient>, project: Entity<Project>) -> Self {
        Self {
            cursor_client,
            project,
            buffer_id: None,
            completion_text: None,
            pending_refresh: None,
            completion_position: None,
            current_binding_id: None,
            current_request_id: None,
            last_buffer_version: None,
            range_to_replace: None,
        }
    }

    fn build_request(
        &self,
        buffer: &Entity<Buffer>,
        cursor_position: Anchor,
        workspace_id: String,
        cx: &App,
    ) -> Option<StreamCppRequest> {
        let buffer_read = buffer.read(cx);
        let snapshot = buffer_read.snapshot();
        
        let file_path = buffer_read
            .file()
            .and_then(|file| Some(file.as_local()?.abs_path(cx)))
            .unwrap_or_else(|| std::path::PathBuf::from("untitled"))
            .display()
            .to_string();
        
        let workspace_root_path = self.project.read(cx)
            .worktrees(cx)
            .next()
            .and_then(|tree| tree.read(cx).abs_path().to_str().map(String::from))
            .unwrap_or_default();
        
        let contents = buffer_read.text();
        let cursor_point = cursor_position.to_point(&snapshot);
        
        let language_id = buffer_read
            .language()
            .map(|lang| lang.name().to_string())
            .unwrap_or_else(|| "plaintext".to_string());
        
        let total_lines = snapshot.max_point().row as i32 + 1;
        
        let current_file = CurrentFileInfo {
            relative_workspace_path: file_path.clone(),
            contents: contents.clone(),
            cursor_position: Some(CursorPosition {
                line: cursor_point.row as i32,
                column: cursor_point.column as i32,
            }),
            dataframes: vec![],
            language_id,
            selection: None,
            diagnostics: vec![],
            total_number_of_lines: total_lines,
            contents_start_at_line: 0,
            top_chunks: vec![],
            alternative_version_id: None,
            file_version: None,
            cell_start_lines: vec![],
            sha_256_hash: None,
            rely_on_filesync: false,
            workspace_root_path: workspace_root_path.clone(),
            line_ending: None,
        };

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;

        let request = StreamCppRequest {
            current_file: Some(current_file),
            diff_history: vec![],
            model_name: None,
            linter_errors: None,
            diff_history_keys: vec![],
            give_debug_output: None,
            file_diff_histories: vec![],
            merged_diff_histories: vec![],
            block_diff_patches: vec![],
            is_nightly: None,
            is_debug: None,
            immediately_ack: None,
            context_items: vec![],
            parameter_hints: vec![],
            lsp_contexts: vec![],
            cpp_intent_info: None,
            enable_more_context: None,
            workspace_id: Some(workspace_id),
            additional_files: vec![],
            control_token: None,
            client_time: Some(now_ms),
            filesync_updates: vec![],
            time_since_request_start: 0.0,
            time_at_request_send: now_ms,
            client_timezone_offset: None,
            lsp_suggested_items: None,
            supports_cpt: None,
            supports_crlf_cpt: None,
            code_results: vec![],
        };

        Some(request)
    }
}

fn completion_from_diff(
    snapshot: BufferSnapshot,
    completion_text: &str,
    position: Anchor,
    delete_range: Range<Anchor>,
) -> EditPrediction {
    let buffer_text = snapshot.text_for_range(delete_range).collect::<String>();

    let mut edits: Vec<(Range<language::Anchor>, Arc<str>)> = Vec::new();

    let completion_graphemes: Vec<&str> = completion_text.graphemes(true).collect();
    let buffer_graphemes: Vec<&str> = buffer_text.graphemes(true).collect();

    let mut offset = position.to_offset(&snapshot);

    let mut i = 0;
    let mut j = 0;
    while i < completion_graphemes.len() && j < buffer_graphemes.len() {
        let k = completion_graphemes[i..]
            .iter()
            .position(|c| *c == buffer_graphemes[j]);
        match k {
            Some(k) => {
                if k != 0 {
                    let offset = snapshot.anchor_after(offset);
                    let edit = (
                        offset..offset,
                        completion_graphemes[i..i + k].join("").into(),
                    );
                    edits.push(edit);
                }
                i += k + 1;
                j += 1;
                offset.add_assign(buffer_graphemes[j - 1].len());
            }
            None => {
                break;
            }
        }
    }

    if j == buffer_graphemes.len() && i < completion_graphemes.len() {
        let offset = snapshot.anchor_after(offset);
        let edit_range = offset..offset;
        let edit_text = completion_graphemes[i..].join("");
        edits.push((edit_range, edit_text.into()));
    }

    EditPrediction::Local {
        id: None,
        edits,
        edit_preview: None,
    }
}

fn reset_completion_cache(
    provider: &mut CursorCompletionProvider,
    _cx: &mut Context<CursorCompletionProvider>,
) {
    provider.pending_refresh = None;
    provider.completion_text = None;
    provider.completion_position = None;
    provider.buffer_id = None;
    provider.current_binding_id = None;
    provider.current_request_id = None;
    provider.last_buffer_version = None;
    provider.range_to_replace = None;
}

fn trim_to_end_of_line_unless_leading_newline(text: &str) -> &str {
    if has_leading_newline(text) {
        text
    } else if let Some(i) = text.find('\n') {
        &text[..i]
    } else {
        text
    }
}

fn has_leading_newline(text: &str) -> bool {
    for c in text.chars() {
        if c == '\n' {
            return true;
        }
        if !c.is_whitespace() {
            return false;
        }
    }
    false
}

impl EditPredictionProvider for CursorCompletionProvider {
    fn name() -> &'static str {
        "cursor"
    }

    fn display_name() -> &'static str {
        "Cursor"
    }

    fn show_completions_in_menu() -> bool {
        true
    }

    fn show_tab_accept_marker() -> bool {
        true
    }

    fn supports_jump_to_edit() -> bool {
        false
    }

    fn is_enabled(&self, _buffer: &Entity<Buffer>, _cursor_position: Anchor, _cx: &App) -> bool {
        true
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_refresh.is_some() && self.completion_text.is_none()
    }

    fn refresh(
        &mut self,
        buffer_handle: Entity<Buffer>,
        cursor_position: Anchor,
        debounce: bool,
        cx: &mut Context<Self>,
    ) {
        log::info!("CursorCompletionProvider::refresh() called, debounce={}", debounce);
        
        if ENABLE_CACHING {
            let buffer_version = buffer_handle.read(cx).snapshot().version().clone();
            let buffer_changed = self.last_buffer_version.as_ref() != Some(&buffer_version);
            
            if buffer_changed {
                log::info!("Buffer version changed, resetting cache");
                reset_completion_cache(self, cx);
            } else if self.completion_text.is_some() {
                log::info!("Buffer unchanged and completion cached, skipping refresh");
                return;
            }
        } else {
            log::info!("Caching disabled, always resetting");
            reset_completion_cache(self, cx);
        }

        let cursor_client = self.cursor_client.clone();
        let workspace_id = cursor_client.read(cx).workspace_id.clone();

        let Some(request) = self.build_request(&buffer_handle, cursor_position, workspace_id.clone(), cx) else {
            return;
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        log::info!("Building completion request and spawning async task");
        self.pending_refresh = Some(cx.spawn(async move |this, cx| {
            let (http_client, auth_token, session_id, client_key, fs_client_key) = cursor_client.update(cx, |client, _cx| {
                (
                    client.http_client(), 
                    client.auth_token.clone(),
                    client.session_id.clone(),
                    client.client_key.clone(),
                    client.fs_client_key.clone(),
                )
            })?;

            log::info!("Requesting completion from Cursor API, auth_token present: {}", auth_token.is_some());
            let (completion_text, binding_id, range_to_replace) = match CursorClient::get_completion(
                http_client, 
                auth_token, 
                session_id,
                client_key,
                fs_client_key,
                request_id.clone(),
                request
            ).await {
                Ok(result) => {
                    log::info!("Received completion text: {} chars, binding_id: {:?}, range: {:?}", result.0.len(), result.1, result.2);
                    result
                }
                Err(e) => {
                    log::error!("Failed to get completion from Cursor API: {:?}", e);
                    return Err(e);
                }
            };

            this.update(cx, |this, cx| {
                this.completion_text = Some(completion_text);
                this.completion_position = Some(cursor_position);
                this.buffer_id = Some(buffer_handle.entity_id());
                this.current_binding_id = binding_id;
                this.current_request_id = Some(request_id);
                this.range_to_replace = range_to_replace;
                if ENABLE_CACHING {
                    this.last_buffer_version = Some(buffer_handle.read(cx).snapshot().version().clone());
                }
                log::info!("Completion cached and notifying UI");
                cx.notify();
            })?;

            log::info!("Completion refresh completed successfully");
            Ok(())
        }));
    }

    fn cycle(
        &mut self,
        _buffer: Entity<Buffer>,
        _cursor_position: Anchor,
        _direction: Direction,
        _cx: &mut Context<Self>,
    ) {
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        if ENABLE_FATE_TRACKING {
            if let Some(request_id) = self.current_request_id.take() {
                let cursor_client = self.cursor_client.clone();
                cx.spawn(async move |_this, cx| {
                    let (http_client, auth_token, session_id, client_key, fs_client_key) = cursor_client.update(cx, |client, _cx| {
                        (
                            client.http_client(),
                            client.auth_token.clone(),
                            client.session_id.clone(),
                            client.client_key.clone(),
                            client.fs_client_key.clone(),
                        )
                    })?;

                    CursorClient::record_cpp_fate(
                        http_client,
                        auth_token,
                        session_id,
                        client_key,
                        fs_client_key,
                        request_id,
                        CppFate::Accept,
                    ).await.ok();
                    Ok::<(), anyhow::Error>(())
                }).detach();
            }
        }
        reset_completion_cache(self, cx);
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        if ENABLE_FATE_TRACKING {
            if let Some(request_id) = self.current_request_id.take() {
                let cursor_client = self.cursor_client.clone();
                cx.spawn(async move |_this, cx| {
                    let (http_client, auth_token, session_id, client_key, fs_client_key) = cursor_client.update(cx, |client, _cx| {
                        (
                            client.http_client(),
                            client.auth_token.clone(),
                            client.session_id.clone(),
                            client.client_key.clone(),
                            client.fs_client_key.clone(),
                        )
                    })?;

                    CursorClient::record_cpp_fate(
                        http_client,
                        auth_token,
                        session_id,
                        client_key,
                        fs_client_key,
                        request_id,
                        CppFate::Reject,
                    ).await.ok();
                    Ok::<(), anyhow::Error>(())
                }).detach();
            }
        }
        reset_completion_cache(self, cx);
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor_position: Anchor,
        _cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        if self.buffer_id != Some(buffer.entity_id()) {
            return None;
        }

        let completion_text = self.completion_text.as_ref()?;

        if let Some(completion_position) = self.completion_position {
            if cursor_position != completion_position {
                return None;
            }
        } else {
            return None;
        }

        let snapshot = buffer.read(_cx).snapshot();

        // If we have a range_to_replace, use greedy grapheme matching
        if let Some(range) = &self.range_to_replace {
            log::info!("Using range_to_replace: start_line={}, end_line={}", 
                       range.start_line_number, range.end_line_number_inclusive);
            
            // Convert line numbers to anchors (lines are 1-indexed in proto, 0-indexed in buffer)
            let start_line = (range.start_line_number - 1) as u32;
            let end_line = (range.end_line_number_inclusive - 1) as u32;
            
            let start_anchor = snapshot.anchor_before(Point::new(start_line, 0));
            let end_point = Point::new(end_line, snapshot.line_len(end_line));
            let end_anchor = snapshot.anchor_after(end_point);
            
            log::info!("Applying completion_from_diff for range");
            
            return Some(completion_from_diff(
                snapshot,
                completion_text,
                start_anchor,
                start_anchor..end_anchor,
            ));
        }

        // Fall back to cursor-based completion
        let completion_text = trim_to_end_of_line_unless_leading_newline(completion_text);
        let completion_text = completion_text.trim_end();

        if !completion_text.trim().is_empty() {
            let cursor_point = cursor_position.to_point(&snapshot);
            let end_of_line = snapshot.anchor_after(language::Point::new(
                cursor_point.row,
                snapshot.line_len(cursor_point.row),
            ));
            let delete_range = cursor_position..end_of_line;

            Some(completion_from_diff(
                snapshot,
                completion_text,
                cursor_position,
                delete_range,
            ))
        } else {
            None
        }
    }
}
