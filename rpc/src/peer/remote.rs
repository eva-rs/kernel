//! Поток, открытый у пира: элементы из буфера приёма, конец из слота.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures::Stream;
use tokio::{
    sync::mpsc,
    time::{Instant, Sleep},
};

use eva_protocol::wire::{ErrorCode, RpcError, frame::Payload};

use crate::{
    codec,
    peer::inner::{End, Event, Guard, after},
};

/// Элементы потока пира; `Err` — последний элемент. Drop до конца шлёт `cancel`.
pub struct RemoteStream {
    id: u64,
    rx: mpsc::Receiver<Event>,
    end: Arc<End>,
    guard: Guard,
    idle: Pin<Box<Sleep>>,
    idle_for: Duration,
    peeked: Option<Payload>,
    done: bool,
}

impl RemoteStream {
    pub(super) fn new(
        id: u64,
        rx: mpsc::Receiver<Event>,
        end: Arc<End>,
        guard: Guard,
        idle_for: Duration,
    ) -> Self {
        Self {
            id,
            rx,
            end,
            guard,
            idle: Box::pin(tokio::time::sleep(idle_for)),
            idle_for,
            peeked: None,
            done: false,
        }
    }

    /// `request.id` потока.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn abandon(&mut self) {
        self.done = true;
        self.guard.abandon();
    }

    /// Ждёт первый кадр: элемент, heartbeat открытия или конец.
    pub(super) async fn first(&mut self) -> Result<(), RpcError> {
        match self.rx.recv().await {
            Some(Event::Item(item)) => {
                self.peeked = Some(item);
                Ok(())
            }
            Some(Event::Heartbeat) => Ok(()),
            None => match self.finish() {
                Some(error) => Err(error),
                None => Ok(()),
            },
        }
    }

    /// Поток кончился со стороны пира.
    fn finish(&mut self) -> Option<RpcError> {
        self.done = true;
        self.guard.disarm();
        match self.end.get() {
            Some(end) => end.clone(),
            None => Some(codec::unavailable("stream interrupted")),
        }
    }

    fn reset_idle(&mut self) {
        let at = after(Instant::now(), self.idle_for);
        self.idle.as_mut().reset(at);
    }
}

impl Stream for RemoteStream {
    type Item = Result<Payload, RpcError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if let Some(item) = this.peeked.take() {
            return Poll::Ready(Some(Ok(item)));
        }

        loop {
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(Event::Item(item))) => {
                    this.reset_idle();
                    return Poll::Ready(Some(Ok(item)));
                }
                Poll::Ready(Some(Event::Heartbeat)) => this.reset_idle(),
                Poll::Ready(None) => return Poll::Ready(this.finish().map(Err)),
                Poll::Pending => break,
            }
        }

        if this.idle.as_mut().poll(cx).is_ready() {
            this.done = true;
            this.guard.fire();
            return Poll::Ready(Some(Err(RpcError::new(
                ErrorCode::Timeout,
                "stream idle for longer than stream_idle_timeout",
            ))));
        }

        Poll::Pending
    }
}
