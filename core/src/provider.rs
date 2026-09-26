//! Направление ядро → провайдер: трейт вендорского крейта и приёмник потока.

use std::{fmt, future::poll_fn, pin::Pin, task};

use eva::auto_impl;
use futures_sink::Sink;

use eva_protocol::types::{
    generate::{EmbedRequest, EmbedResponse, GenItem, GenerateRequest, ProviderError},
    model::{ModelDecl, ProviderDecl},
};

/// Провайдер инференса: крейт `providers/<vendor>` или прокси модуля.
///
/// Реестр моделей и факты эмбеддера ядро берёт только из `decl()`/`models()`.
#[auto_impl(&, Arc)]
pub trait Provider: Send + Sync {
    fn decl(&self) -> ProviderDecl;

    fn models(&self) -> Vec<ModelDecl>;

    /// Льёт элементы в `sink`; `Stop` — последний элемент перед `Ok`.
    fn generate(
        &self,
        req: GenerateRequest,
        sink: GenSink,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send;

    fn embed(
        &self,
        req: EmbedRequest,
    ) -> impl Future<Output = Result<EmbedResponse, ProviderError>> + Send;

    /// Зовётся только при `decl().count_tokens`.
    fn count_tokens(
        &self,
        req: GenerateRequest,
    ) -> impl Future<Output = Result<u32, ProviderError>> + Send;
}

/// Потребитель потока ушёл: генерацию пора бросить.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("generation consumer is gone")
    }
}

impl std::error::Error for Closed {}

/// Приёмник `GenItem`: канал тура в ядре либо стрим к модулю в `eva-rpc`.
pub struct GenSink(Pin<Box<dyn Sink<GenItem, Error = Closed> + Send>>);

impl GenSink {
    /// Любая ошибка приёмника означает, что потребитель ушёл.
    pub fn new<S>(sink: S) -> Self
    where
        S: Sink<GenItem> + Send + 'static,
    {
        Self(Box::pin(AnyClosed(Box::pin(sink))))
    }

    /// Отдаёт элемент сразу: дельты текста нужны клиенту живыми.
    pub async fn push(&mut self, item: GenItem) -> Result<(), Closed> {
        poll_fn(|cx| self.0.as_mut().poll_ready(cx)).await?;
        self.0.as_mut().start_send(item)?;
        poll_fn(|cx| self.0.as_mut().poll_flush(cx)).await
    }
}

impl fmt::Debug for GenSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenSink").finish_non_exhaustive()
    }
}

struct AnyClosed<S>(Pin<Box<S>>);

impl<S: Sink<GenItem>> Sink<GenItem> for AnyClosed<S> {
    type Error = Closed;

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), Closed>> {
        self.get_mut().0.as_mut().poll_ready(cx).map_err(|_| Closed)
    }

    fn start_send(self: Pin<&mut Self>, item: GenItem) -> Result<(), Closed> {
        self.get_mut()
            .0
            .as_mut()
            .start_send(item)
            .map_err(|_| Closed)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), Closed>> {
        self.get_mut().0.as_mut().poll_flush(cx).map_err(|_| Closed)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), Closed>> {
        self.get_mut().0.as_mut().poll_close(cx).map_err(|_| Closed)
    }
}
