//! Направление ядро → модуль (`06 §4`, `§7`): не операции, а прокси трейтов
//! ядра. `ModuleClient` — сторона ядра (`tools.call`, `hooks.<point>`,
//! `events.publish`, `ModuleProvider` для `provider.*`), `ModuleHandler` —
//! сторона SDK.

use std::{
    marker::PhantomData,
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures::{Stream, StreamExt as _};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::mpsc;

use eva_core::{GenSink, Provider};
use eva_protocol::{
    types::{
        generate::{
            EmbedRequest, EmbedResponse, GenItem, GenerateRequest, ProviderError, ProviderErrorKind,
        },
        model::{ModelDecl, ProviderDecl},
    },
    wire::{
        ErrorCode, RpcError,
        capabilities::ProviderCap,
        events::Publish,
        frame::Payload,
        hooks::{AfterTool, BeforeTool, BeforeTurn, Decision, Hook, HookPoint, NoPatch},
        method,
        provider::TokenCount,
        tools::{ToolCall, ToolItem, ToolResult},
    },
};

use crate::{
    codec,
    peer::{Call, Kind, Outgoing, Peer, RemoteStream, Service},
};

/// Вызовы ядра в сессию модуля.
#[derive(Debug, Clone)]
pub struct ModuleClient {
    peer: Peer,
}

impl ModuleClient {
    pub fn new(peer: Peer) -> Self {
        Self { peer }
    }

    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// `tools.call`: `progress`* и один `result`; ошибка потока — `Fatal` тура.
    pub async fn call_tool(&self, call: &ToolCall) -> Result<Typed<ToolItem>, RpcError> {
        let params = codec::encode(call).map_err(codec::bad_params)?;
        let remote = self.peer.open(method::TOOLS_CALL, params).await?;
        Ok(Typed::new(remote))
    }

    /// `hooks.<point>` с таймаутом хука модуля; решение по `fail_mode` — дело ядра.
    pub async fn hook<H>(&self, input: &H, limit: Duration) -> Result<Decision<H::Patch>, RpcError>
    where
        H: Hook + Serialize,
        H::Patch: DeserializeOwned,
    {
        let params = codec::encode(input).map_err(codec::bad_params)?;
        let raw = self
            .peer
            .call_timeout(H::POINT.method(), params, limit)
            .await?;
        codec::decode(&raw).map_err(codec::bad_result)
    }

    /// `notify events.publish`; порядок публикаций сохраняется.
    pub async fn publish(&self, event: &Publish) -> Result<(), RpcError> {
        let params = codec::encode(event).map_err(codec::bad_params)?;
        self.peer.notify(method::EVENTS_PUBLISH, params).await
    }

    /// Провайдер запасного входа поверх этой сессии.
    pub fn provider(&self, cap: ProviderCap) -> ModuleProvider {
        ModuleProvider::new(self.clone(), cap)
    }
}

/// Поток пира типом элемента; `Err` — последний элемент.
pub struct Typed<T> {
    remote: RemoteStream,
    done: bool,
    _item: PhantomData<fn() -> T>,
}

impl<T> Typed<T> {
    fn new(remote: RemoteStream) -> Self {
        Self {
            remote,
            done: false,
            _item: PhantomData,
        }
    }
}

impl<T: DeserializeOwned> Stream for Typed<T> {
    type Item = Result<T, RpcError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let item = match this.remote.poll_next_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Ready(Some(item)) => {
                item.and_then(|raw| codec::decode(&raw).map_err(codec::bad_result))
            }
        };
        this.done = item.is_err();
        Poll::Ready(Some(item))
    }
}

/// Провайдер модуля (`06 §3`): тот же трейт, что у крейтов `providers/*`,
/// поток `provider.generate` льётся в `GenSink`.
#[derive(Debug, Clone)]
pub struct ModuleProvider {
    module: ModuleClient,
    decl: ProviderDecl,
    models: Vec<ModelDecl>,
}

impl ModuleProvider {
    /// Модели — из capability handshake.
    pub fn new(module: ModuleClient, cap: ProviderCap) -> Self {
        let ProviderCap { decl, models } = cap;
        Self {
            module,
            decl,
            models,
        }
    }
}

impl Provider for ModuleProvider {
    fn decl(&self) -> ProviderDecl {
        self.decl
    }

    fn models(&self) -> Vec<ModelDecl> {
        self.models.clone()
    }

    async fn generate(&self, req: GenerateRequest, mut sink: GenSink) -> Result<(), ProviderError> {
        let params = codec::encode(&req).map_err(|e| provider_error(codec::bad_params(e)))?;
        let mut items = Typed::<GenItem>::new(
            self.module
                .peer
                .open(method::PROVIDER_GENERATE, params)
                .await
                .map_err(provider_error)?,
        );

        while let Some(item) = items.next().await {
            let item = item.map_err(provider_error)?;
            // Потребитель ушёл: drop потока отменит генерацию у модуля.
            if sink.push(item).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        let params = codec::encode(&req).map_err(|e| provider_error(codec::bad_params(e)))?;
        let raw = self
            .module
            .peer
            .call(method::PROVIDER_EMBED, params)
            .await
            .map_err(provider_error)?;
        codec::decode(&raw).map_err(|e| provider_error(codec::bad_result(e)))
    }

    async fn count_tokens(&self, req: GenerateRequest) -> Result<u32, ProviderError> {
        let params = codec::encode(&req).map_err(|e| provider_error(codec::bad_params(e)))?;
        let raw = self
            .module
            .peer
            .call(method::PROVIDER_COUNT_TOKENS, params)
            .await
            .map_err(provider_error)?;
        codec::decode::<TokenCount>(&raw)
            .map(|count| count.tokens)
            .map_err(|e| provider_error(codec::bad_result(e)))
    }
}

/// `data` модуля, если это `ProviderError`, иначе вид по коду транспорта:
/// обрыв и таймаут — `transient`, их ретраит fallback-цепочка ядра.
pub fn provider_error(error: RpcError) -> ProviderError {
    if let Some(typed) = error
        .data
        .as_ref()
        .and_then(|data| codec::decode::<ProviderError>(data).ok())
    {
        return typed;
    }

    let kind = match error.code {
        ErrorCode::Timeout | ErrorCode::Cancelled | ErrorCode::Unavailable => {
            ProviderErrorKind::Transient
        }
        ErrorCode::NotFound => ProviderErrorKind::NotFound,
        ErrorCode::Quota => ProviderErrorKind::Quota,
        ErrorCode::Unauthorized | ErrorCode::Forbidden => ProviderErrorKind::Auth,
        ErrorCode::InvalidParams | ErrorCode::UnknownMethod | ErrorCode::ProtoVersion => {
            ProviderErrorKind::Invalid
        }
        ErrorCode::Conflict | ErrorCode::Internal | ErrorCode::Unknown => ProviderErrorKind::Other,
    };
    ProviderError {
        kind,
        http_status: None,
        raw: error.message,
        retry_after: error.retry_after,
    }
}

/// Прогресс тула: `progress{text}` в поток `tools.call`. Отмену тул видит
/// по drop своего future, поэтому ушедший потребитель тут молча теряет текст.
#[derive(Debug, Clone)]
pub struct Progress(mpsc::Sender<String>);

impl Progress {
    pub async fn send(&self, text: impl Into<String>) {
        _ = self.0.send(text.into()).await;
    }
}

/// Обработчики модуля для вызовов ядра. Необъявленное отвечает
/// `unknown_method` (провайдер — `ProviderError{invalid}`); дедуп по
/// `call_id` и прочая защита от повторов — забота реализации.
pub trait ModuleHandler: Send + Sync + 'static {
    fn call_tool(
        &self,
        call: ToolCall,
        progress: Progress,
    ) -> impl Future<Output = Result<ToolResult, RpcError>> + Send {
        drop((call, progress));
        async { Err(RpcError::unknown_method(method::TOOLS_CALL)) }
    }

    fn before_turn(
        &self,
        input: BeforeTurn,
    ) -> impl Future<Output = Result<Decision<NoPatch>, RpcError>> + Send {
        drop(input);
        async { Err(RpcError::unknown_method(HookPoint::BeforeTurn.method())) }
    }

    fn before_tool(
        &self,
        input: BeforeTool,
    ) -> impl Future<Output = Result<Decision<NoPatch>, RpcError>> + Send {
        drop(input);
        async { Err(RpcError::unknown_method(HookPoint::BeforeTool.method())) }
    }

    fn after_tool(
        &self,
        input: AfterTool,
    ) -> impl Future<Output = Result<Decision<ToolResult>, RpcError>> + Send {
        drop(input);
        async { Err(RpcError::unknown_method(HookPoint::AfterTool.method())) }
    }

    /// `events.publish`: по одному, в порядке публикации.
    fn event(&self, event: Publish) -> impl Future<Output = ()> + Send {
        drop(event);
        async {}
    }

    /// Запасной вход провайдера: та же форма, что `Provider::generate`.
    fn generate(
        &self,
        req: GenerateRequest,
        sink: GenSink,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send {
        drop((req, sink));
        async { Err(not_a_provider()) }
    }

    fn embed(
        &self,
        req: EmbedRequest,
    ) -> impl Future<Output = Result<EmbedResponse, ProviderError>> + Send {
        drop(req);
        async { Err(not_a_provider()) }
    }

    fn count_tokens(
        &self,
        req: GenerateRequest,
    ) -> impl Future<Output = Result<u32, ProviderError>> + Send {
        drop(req);
        async { Err(not_a_provider()) }
    }
}

fn not_a_provider() -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Invalid,
        http_status: None,
        raw: "the module declares no provider capability".to_owned(),
        retry_after: None,
    }
}

/// Модуль без обработчиков: только вызовы модуль → ядро.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHandler;

impl ModuleHandler for NoHandler {}

/// `Service` пира модуля поверх `ModuleHandler`.
pub(crate) struct ModuleService<H> {
    handler: Arc<H>,
    buffer: usize,
}

impl<H> ModuleService<H> {
    pub fn new(handler: H, buffer: usize) -> Self {
        Self {
            handler: Arc::new(handler),
            buffer,
        }
    }
}

fn hook_point(method: &str) -> Option<HookPoint> {
    HookPoint::ALL
        .into_iter()
        .find(|point| point.method() == method)
}

fn encode_item<T: Serialize>(item: &T) -> Result<Payload, RpcError> {
    codec::encode(item).map_err(|e| RpcError::new(ErrorCode::Internal, e))
}

async fn hook<I, P, F>(params: &Payload, run: impl FnOnce(I) -> F) -> Result<Payload, RpcError>
where
    I: DeserializeOwned,
    P: Serialize,
    F: Future<Output = Result<Decision<P>, RpcError>>,
{
    let input = codec::decode(params).map_err(codec::bad_params)?;
    encode_item(&run(input).await?)
}

impl<H: ModuleHandler> Service for ModuleService<H> {
    fn kind(&self, method: &str) -> Option<Kind> {
        match method {
            method::TOOLS_CALL | method::PROVIDER_GENERATE => Some(Kind::Stream),
            method::PROVIDER_EMBED | method::PROVIDER_COUNT_TOKENS => Some(Kind::Unary),
            other => hook_point(other).map(|_| Kind::Unary),
        }
    }

    async fn unary(&self, call: Call) -> Result<Payload, RpcError> {
        let Call { method, params, .. } = call;
        let handler = &*self.handler;

        match method.as_str() {
            method::PROVIDER_EMBED => {
                let req = codec::decode(&params).map_err(codec::bad_params)?;
                let response = handler.embed(req).await.map_err(|e| codec::typed(&e))?;
                encode_item(&response)
            }
            method::PROVIDER_COUNT_TOKENS => {
                let req = codec::decode(&params).map_err(codec::bad_params)?;
                let tokens = handler
                    .count_tokens(req)
                    .await
                    .map_err(|e| codec::typed(&e))?;
                encode_item(&TokenCount { tokens })
            }
            other => match hook_point(other) {
                Some(HookPoint::BeforeTurn) => hook(&params, |i| handler.before_turn(i)).await,
                Some(HookPoint::BeforeTool) => hook(&params, |i| handler.before_tool(i)).await,
                Some(HookPoint::AfterTool) => hook(&params, |i| handler.after_tool(i)).await,
                Some(HookPoint::Unknown) | None => Err(RpcError::unknown_method(other)),
            },
        }
    }

    async fn stream(&self, call: Call) -> Result<Outgoing, RpcError> {
        let Call { method, params, .. } = call;
        match method.as_str() {
            method::TOOLS_CALL => {
                let call = codec::decode(&params).map_err(codec::bad_params)?;
                Ok(tool_stream(self.handler.clone(), call, self.buffer))
            }
            method::PROVIDER_GENERATE => {
                let req = codec::decode(&params).map_err(codec::bad_params)?;
                Ok(generate_stream(self.handler.clone(), req, self.buffer))
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }

    async fn notify(&self, method: String, params: Payload) {
        if method != method::EVENTS_PUBLISH {
            log::debug!(method = method.as_str(); "notification without a handler");
            return;
        }
        match codec::decode::<Publish>(&params) {
            Ok(event) => self.handler.event(event).await,
            Err(e) => log::warn!(error:% = e; "malformed events.publish dropped"),
        }
    }
}

/// Шаг совместной прокрутки обработчика и его канала.
enum Step<T, O> {
    Item(Option<T>),
    Done(O),
}

fn tool_stream<H: ModuleHandler>(handler: Arc<H>, call: ToolCall, buffer: usize) -> Outgoing {
    let (tx, mut rx) = mpsc::channel::<String>(buffer);
    Box::pin(async_stream::stream! {
        let mut run = pin!(handler.call_tool(call, Progress(tx)));
        let result = loop {
            let step = tokio::select! {
                biased;
                text = rx.recv() => Step::Item(text),
                result = &mut run => Step::Done(result),
            };
            match step {
                Step::Item(Some(text)) => yield encode_item(&ToolItem::Progress { text }),
                Step::Item(None) => break run.await,
                Step::Done(result) => break result,
            }
        };
        while let Ok(text) = rx.try_recv() {
            yield encode_item(&ToolItem::Progress { text });
        }
        match result {
            Ok(result) => yield encode_item(&ToolItem::Result(result)),
            Err(error) => yield Err(error),
        }
    })
}

fn generate_stream<H: ModuleHandler>(
    handler: Arc<H>,
    req: GenerateRequest,
    buffer: usize,
) -> Outgoing {
    let (tx, mut rx) = futures::channel::mpsc::channel::<GenItem>(buffer);
    Box::pin(async_stream::stream! {
        let mut run = pin!(handler.generate(req, GenSink::new(tx)));
        let result = loop {
            let step = tokio::select! {
                biased;
                item = rx.next() => Step::Item(item),
                result = &mut run => Step::Done(result),
            };
            match step {
                Step::Item(Some(item)) => yield encode_item(&item),
                Step::Item(None) => break run.await,
                Step::Done(result) => break result,
            }
        };
        while let Ok(item) = rx.try_recv() {
            yield encode_item(&item);
        }
        if let Err(error) = result {
            yield Err(codec::typed(&error));
        }
    })
}
