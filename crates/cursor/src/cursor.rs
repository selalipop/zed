mod cursor_client;
mod cursor_completion_provider;

pub use cursor_client::CursorClient;
pub use cursor_completion_provider::CursorCompletionProvider;

use gpui::{App, AppContext, Entity, Global};
use std::sync::Arc;
use http_client::HttpClient;

#[derive(Clone)]
struct CursorGlobal(Entity<CursorClient>);

impl Global for CursorGlobal {}

impl CursorClient {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        let result = cx.try_global::<CursorGlobal>()
            .map(|global| global.0.clone());
        log::info!("CursorClient::global() called, result: {}", result.is_some());
        result
    }

    pub fn set_global(client: Entity<Self>, cx: &mut App) {
        log::info!("CursorClient::set_global() called - setting global entity");
        cx.set_global(CursorGlobal(client));
    }
}

pub fn init(http_client: Arc<dyn HttpClient>, cx: &mut App) {
    log::info!("cursor::init() called - initializing CursorClient");
    let cursor_client = cx.new(|_| CursorClient::new(http_client));
    CursorClient::set_global(cursor_client.clone(), cx);
    log::info!("cursor::init() completed - CursorClient global entity set");
}
