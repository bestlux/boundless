use std::{
    pin::Pin,
    task::{Context, Poll},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::{Stream, wrappers::BroadcastStream};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppEvent {
    pub kind: String,
    pub detail: String,
    pub emitted_at: String,
}

impl AppEvent {
    pub fn new(kind: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            detail: detail.into(),
            emitted_at: Utc::now().to_rfc3339(),
        }
    }
}

pub struct AppEventStream {
    inner: BroadcastStream<AppEvent>,
}

impl AppEventStream {
    pub fn new(receiver: broadcast::Receiver<AppEvent>) -> Self {
        Self {
            inner: BroadcastStream::new(receiver),
        }
    }
}

impl Stream for AppEventStream {
    type Item = Result<AppEvent, broadcast::error::RecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(Ok(event))),
            Poll::Ready(Some(Err(
                tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(count),
            ))) => Poll::Ready(Some(Err(broadcast::error::RecvError::Lagged(count)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
