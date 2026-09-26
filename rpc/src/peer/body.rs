//! Тело входящей загрузки: чанки из буфера приёма как `ByteStream`.

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

use eva_protocol::{errors::Aux, types::Bytes};

use crate::{
    codec,
    peer::inner::{End, Event, after},
};

pub(super) struct Body {
    rx: mpsc::Receiver<Event>,
    end: Arc<End>,
    idle: Pin<Box<Sleep>>,
    idle_for: Duration,
    done: bool,
}

impl Body {
    pub fn new(rx: mpsc::Receiver<Event>, end: Arc<End>, idle_for: Duration) -> Self {
        Self {
            rx,
            end,
            idle: Box::pin(tokio::time::sleep(idle_for)),
            idle_for,
            done: false,
        }
    }

    fn fail(&mut self, aux: Aux) -> Poll<Option<Result<eva::bytes::Bytes, Aux>>> {
        self.done = true;
        Poll::Ready(Some(Err(aux)))
    }
}

impl Stream for Body {
    type Item = Result<eva::bytes::Bytes, Aux>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }

        loop {
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(Event::Item(raw))) => {
                    let at = after(Instant::now(), this.idle_for);
                    this.idle.as_mut().reset(at);
                    return match codec::decode::<Bytes>(&raw) {
                        Ok(chunk) => Poll::Ready(Some(Ok(chunk.into()))),
                        Err(e) => this.fail(Aux::Invalid(format!("malformed upload chunk: {e}"))),
                    };
                }
                Poll::Ready(Some(Event::Heartbeat)) => {
                    let at = after(Instant::now(), this.idle_for);
                    this.idle.as_mut().reset(at);
                }
                Poll::Ready(None) => {
                    return match this.end.get() {
                        Some(None) => {
                            this.done = true;
                            Poll::Ready(None)
                        }
                        Some(Some(error)) => {
                            let aux = codec::into_aux(error.clone());
                            this.fail(aux)
                        }
                        None => this.fail(Aux::Unavailable("upload body interrupted".to_owned())),
                    };
                }
                Poll::Pending => break,
            }
        }

        if this.idle.as_mut().poll(cx).is_ready() {
            return this.fail(Aux::Unavailable(
                "upload body idle for longer than stream_idle_timeout".to_owned(),
            ));
        }
        Poll::Pending
    }
}
