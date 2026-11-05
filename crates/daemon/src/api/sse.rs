use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use futures::stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::auth::AuthUser;
use crate::state::AppState;

pub async fn events(
    State(state): State<Arc<AppState>>,
    _user: AuthUser,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|ev| async move {
        match ev {
            Ok(ev) => {
                let name = match &ev {
                    ferry_core::api::Event::JobUpdated { .. } => "job",
                    ferry_core::api::Event::TaskUpdated { .. } => "task",
                    ferry_core::api::Event::JobLog { .. } => "log",
                    ferry_core::api::Event::GpusUpdated => "gpus",
                    ferry_core::api::Event::Heartbeat => "heartbeat",
                };
                Some(Ok(SseEvent::default()
                    .event(name)
                    .json_data(ev)
                    .unwrap_or_else(|_| SseEvent::default())))
            }
            Err(_) => None, // lagged; the client will refetch
        }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
