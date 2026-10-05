//! Вызовы ядро → модуль (`06 §4`, `§7`): `ModuleHandler` поверх
//! зарегистрированных тулов, событий, хуков и провайдера.

use std::{
    any::Any,
    collections::{BTreeMap, HashMap, VecDeque},
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::FutureExt as _;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{sync::watch, task::AbortHandle};

use eva_core::{GenSink, Provider};
use eva_protocol::{
    types::{
        ToolId,
        generate::{
            EmbedRequest, EmbedResponse, GenerateRequest, ProviderError, ProviderErrorKind,
        },
        model::{ModelDecl, ProviderDecl},
    },
    wire::{
        ErrorCode, RpcError, Welcome,
        events::{Event, Publish, Topic},
        hooks::{AfterTool, BeforeTool, BeforeTurn, Decision, Hook, NoPatch},
        tools::{ToolCall, ToolResult},
    },
};
use eva_rpc::{ModuleHandler, Progress, RpcClient};

use crate::{
    Kernel,
    live::Link,
    tool::{BoxFuture, ToolCtx, ToolEntry, canonical},
};

/// Сколько завершённых `tools.call` помнится для повтора с тем же `call_id`.
const DONE_CAPACITY: usize = 256;

/// Отмена и обрыв сокета выглядят одинаково — drop потока; обрыв
/// отличается закрытым соединением, которое пир отмечает чуть позже drop.
const CANCEL_SETTLE: Duration = Duration::from_millis(50);

pub(crate) type EventFn = Arc<dyn Fn(Kernel, Event) -> BoxFuture<()> + Send + Sync>;
pub(crate) type HookFn<H> =
    Arc<dyn Fn(Kernel, H) -> BoxFuture<Decision<<H as Hook>::Patch>> + Send + Sync>;

/// Обработчики хуков по точкам; наружу непрозрачен.
#[derive(Default)]
pub struct HookSlots {
    before_turn: Option<HookFn<BeforeTurn>>,
    before_tool: Option<HookFn<BeforeTool>>,
    after_tool: Option<HookFn<AfterTool>>,
}

/// Вход точки хука, для которой у SDK есть слот; реализован для трёх точек `06 §7`.
pub trait HookInput: Hook + DeserializeOwned + Serialize + Send + Sized + 'static
where
    Self::Patch: Serialize + DeserializeOwned + Send + 'static,
{
    #[doc(hidden)]
    fn slot(slots: &HookSlots) -> Option<&HookFn<Self>>;

    #[doc(hidden)]
    fn slot_mut(slots: &mut HookSlots) -> &mut Option<HookFn<Self>>;
}

macro_rules! hook_input {
    ($($ty:ident => $field:ident),* $(,)?) => {$(
        impl HookInput for $ty {
            fn slot(slots: &HookSlots) -> Option<&HookFn<Self>> {
                slots.$field.as_ref()
            }

            fn slot_mut(slots: &mut HookSlots) -> &mut Option<HookFn<Self>> {
                &mut slots.$field
            }
        }
    )*};
}

hook_input! {
    BeforeTurn => before_turn,
    BeforeTool => before_tool,
    AfterTool => after_tool,
}

type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `Provider` с боксами вместо `impl Future`: трейт ядра не object-safe.
pub(crate) trait DynProvider: Send + Sync {
    fn decl(&self) -> ProviderDecl;
    fn models(&self) -> Vec<ModelDecl>;
    fn generate(
        &self,
        req: GenerateRequest,
        sink: GenSink,
    ) -> LocalFuture<'_, Result<(), ProviderError>>;
    fn embed(&self, req: EmbedRequest) -> LocalFuture<'_, Result<EmbedResponse, ProviderError>>;
    fn count_tokens(&self, req: GenerateRequest) -> LocalFuture<'_, Result<u32, ProviderError>>;
}

impl<P: Provider> DynProvider for P {
    fn decl(&self) -> ProviderDecl {
        Provider::decl(self)
    }

    fn models(&self) -> Vec<ModelDecl> {
        Provider::models(self)
    }

    fn generate(
        &self,
        req: GenerateRequest,
        sink: GenSink,
    ) -> LocalFuture<'_, Result<(), ProviderError>> {
        Box::pin(Provider::generate(self, req, sink))
    }

    fn embed(&self, req: EmbedRequest) -> LocalFuture<'_, Result<EmbedResponse, ProviderError>> {
        Box::pin(Provider::embed(self, req))
    }

    fn count_tokens(&self, req: GenerateRequest) -> LocalFuture<'_, Result<u32, ProviderError>> {
        Box::pin(Provider::count_tokens(self, req))
    }
}

/// Регистрации модуля — общие для всех его соединений.
pub(crate) struct Registry {
    pub tools: BTreeMap<String, ToolEntry>,
    pub events: Vec<(Topic, EventFn)>,
    pub hooks: HookSlots,
    pub provider: Option<Arc<dyn DynProvider>>,
    /// Слабая ссылка: пир держит обработчик, связь держит клиента с пиром.
    pub link: Weak<Link>,
    flights: Mutex<Flights>,
}

impl Registry {
    pub fn new(
        tools: BTreeMap<String, ToolEntry>,
        events: Vec<(Topic, EventFn)>,
        hooks: HookSlots,
        provider: Option<Arc<dyn DynProvider>>,
        link: Weak<Link>,
    ) -> Self {
        Self {
            tools,
            events,
            hooks,
            provider,
            link,
            flights: Mutex::default(),
        }
    }

    fn kernel(&self) -> Result<Kernel, RpcError> {
        self.link
            .upgrade()
            .ok_or_else(|| RpcError::new(ErrorCode::Unavailable, "module is shutting down"))?
            .kernel()
            .ok_or_else(|| RpcError::new(ErrorCode::Unavailable, "module is not connected"))
    }

    fn flights(&self) -> MutexGuard<'_, Flights> {
        self.flights.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Состояние одного соединения: карта `tool_id` из `welcome` и факт обрыва.
#[derive(Default)]
pub(crate) struct ConnState {
    tool_ids: OnceLock<BTreeMap<ToolId, String>>,
    closed: AtomicBool,
}

impl ConnState {
    /// Соединение установлено: карта тулов и сторож закрытия.
    pub fn bind(self: &Arc<Self>, client: &RpcClient, welcome: &Welcome) {
        let ids = welcome
            .tool_ids
            .iter()
            .map(|(path, id)| (*id, path.clone()))
            .collect();
        _ = self.tool_ids.set(ids);

        let peer = client.peer().clone();
        let state = self.clone();
        tokio::spawn(async move {
            peer.closed().await;
            state.closed.store(true, Ordering::Release);
        });
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

type Outcome = Result<ToolResult, RpcError>;

struct Flight {
    rx: watch::Receiver<Option<Outcome>>,
    abort: AbortHandle,
    waiters: AtomicUsize,
}

#[derive(Default)]
struct Flights {
    running: HashMap<String, Arc<Flight>>,
    done: HashMap<String, Outcome>,
    order: VecDeque<String>,
}

impl Flights {
    fn finish(&mut self, call_id: String, outcome: Outcome) {
        self.running.remove(&call_id);
        if self.done.insert(call_id.clone(), outcome).is_none() {
            self.order.push_back(call_id);
        }
        while self.order.len() > DONE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.done.remove(&old);
            }
        }
    }
}

/// Обработчик одного соединения.
#[derive(Clone)]
pub struct SdkHandler {
    registry: Arc<Registry>,
    conn: Arc<ConnState>,
}

impl SdkHandler {
    pub(crate) fn new(registry: Arc<Registry>, conn: Arc<ConnState>) -> Self {
        Self { registry, conn }
    }

    /// Путь тула: по `tool_id` из `welcome`, иначе по каноничному имени.
    fn resolve(&self, call: &ToolCall) -> Option<&ToolEntry> {
        let tools = &self.registry.tools;
        let by_id = call
            .tool_id
            .and_then(|id| self.conn.tool_ids.get()?.get(&id))
            .and_then(|path| tools.get(path));
        by_id.or_else(|| {
            tools
                .iter()
                .find(|(path, _)| canonical(path) == call.canonical)
                .map(|(_, entry)| entry)
        })
    }

    async fn tool(&self, call: ToolCall, progress: Progress) -> Outcome {
        let flight = {
            let mut flights = self.registry.flights();
            if let Some(done) = flights.done.get(&call.call_id) {
                log::debug!(call_id = call.call_id.as_str(); "tools.call repeated, cached result");
                return done.clone();
            }
            match flights.running.get(&call.call_id) {
                Some(flight) => {
                    log::debug!(call_id = call.call_id.as_str(); "tools.call repeated, joined the running call");
                    flight.clone()
                }
                None => {
                    let flight = self.start(call, progress)?;
                    let key = flight.0;
                    flights.running.insert(key, flight.1.clone());
                    flight.1
                }
            }
        };
        flight.waiters.fetch_add(1, Ordering::AcqRel);

        let mut waiter = Waiter {
            flight: flight.clone(),
            conn: self.conn.clone(),
            registry: Arc::downgrade(&self.registry),
            done: false,
        };
        let mut rx = flight.rx.clone();
        let outcome = match rx.wait_for(Option::is_some).await {
            Ok(outcome) => outcome
                .clone()
                .unwrap_or_else(|| Err(RpcError::cancelled())),
            Err(_) => Err(RpcError::cancelled()),
        };
        waiter.done = true;
        outcome
    }

    /// Запуск тула отдельной задачей: обрыв сокета её не убивает, повтор
    /// после `hello.resume` дождётся того же исполнения.
    fn start(&self, call: ToolCall, progress: Progress) -> Result<(String, Arc<Flight>), RpcError> {
        let entry = self.resolve(&call).ok_or_else(|| {
            RpcError::new(
                ErrorCode::NotFound,
                format_args!("unknown tool {}", call.canonical),
            )
        })?;
        let kernel = self.registry.kernel()?;

        let ToolCall {
            call_id,
            input,
            ctx,
            ..
        } = call;
        let run = entry.run.clone();
        let (tx, rx) = watch::channel(None);
        let registry = Arc::downgrade(&self.registry);
        let key = call_id.clone();
        let ctx = ToolCtx::new(call_id.clone(), ctx, kernel, progress);

        let task = tokio::spawn(async move {
            // Паника иначе оставила бы полёт в `running` навсегда, а повтор `call_id` — `cancelled`.
            let outcome = AssertUnwindSafe(async move { run(ctx, input).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| {
                    log::error!(call_id = key.as_str(), panic = panic_message(&*panic); "tool panicked");
                    Err(RpcError::new(ErrorCode::Internal, "tool panicked"))
                });
            if let Some(registry) = registry.upgrade() {
                registry.flights().finish(key, outcome.clone());
            }
            _ = tx.send(Some(outcome));
        });

        Ok((
            call_id,
            Arc::new(Flight {
                rx,
                abort: task.abort_handle(),
                waiters: AtomicUsize::new(0),
            }),
        ))
    }

    async fn event(&self, publish: Publish) {
        let topic = publish.event.topic();
        let handlers: Vec<EventFn> = self
            .registry
            .events
            .iter()
            .filter(|(t, _)| *t == topic)
            .map(|(_, handler)| handler.clone())
            .collect();
        if handlers.is_empty() {
            return;
        }
        let kernel = match self.registry.kernel() {
            Ok(kernel) => kernel,
            Err(e) => {
                log::warn!(topic:% = topic, error:% = e; "event dropped");
                return;
            }
        };
        for handler in handlers {
            handler(kernel.clone(), publish.event.clone()).await;
        }
    }

    async fn hook<H>(&self, input: H) -> Result<Decision<H::Patch>, RpcError>
    where
        H: HookInput,
        H::Patch: Serialize + DeserializeOwned + Send + 'static,
    {
        let handler = H::slot(&self.registry.hooks)
            .cloned()
            .ok_or_else(|| RpcError::unknown_method(H::POINT.method()))?;
        let kernel = self.registry.kernel()?;
        Ok(handler(kernel, input).await)
    }

    fn provider(&self) -> Result<Arc<dyn DynProvider>, ProviderError> {
        self.registry.provider.clone().ok_or_else(|| ProviderError {
            kind: ProviderErrorKind::Invalid,
            http_status: None,
            raw: "the module declares no provider capability".to_owned(),
            retry_after: None,
        })
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// Ожидающий результат тула; брошенный — возможно, отмена.
struct Waiter {
    flight: Arc<Flight>,
    conn: Arc<ConnState>,
    registry: Weak<Registry>,
    done: bool,
}

impl Drop for Waiter {
    fn drop(&mut self) {
        if self.done || self.flight.waiters.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let flight = self.flight.clone();
        let conn = self.conn.clone();
        let registry = self.registry.clone();
        let settle = async move {
            tokio::time::sleep(CANCEL_SETTLE).await;
            if conn.is_closed() || flight.waiters.load(Ordering::Acquire) != 0 {
                return;
            }
            flight.abort.abort();
            if let Some(registry) = registry.upgrade() {
                let mut flights = registry.flights();
                flights
                    .running
                    .retain(|_, running| !Arc::ptr_eq(running, &flight));
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => _ = runtime.spawn(settle),
            Err(_) => self.flight.abort.abort(),
        }
    }
}

impl ModuleHandler for SdkHandler {
    fn call_tool(
        &self,
        call: ToolCall,
        progress: Progress,
    ) -> impl Future<Output = Result<ToolResult, RpcError>> + Send {
        let this = self.clone();
        async move { this.tool(call, progress).await }
    }

    fn before_turn(
        &self,
        input: BeforeTurn,
    ) -> impl Future<Output = Result<Decision<NoPatch>, RpcError>> + Send {
        let this = self.clone();
        async move { this.hook(input).await }
    }

    fn before_tool(
        &self,
        input: BeforeTool,
    ) -> impl Future<Output = Result<Decision<NoPatch>, RpcError>> + Send {
        let this = self.clone();
        async move { this.hook(input).await }
    }

    fn after_tool(
        &self,
        input: AfterTool,
    ) -> impl Future<Output = Result<Decision<ToolResult>, RpcError>> + Send {
        let this = self.clone();
        async move { this.hook(input).await }
    }

    fn event(&self, event: Publish) -> impl Future<Output = ()> + Send {
        let this = self.clone();
        async move { this.event(event).await }
    }

    fn generate(
        &self,
        req: GenerateRequest,
        sink: GenSink,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send {
        let provider = self.provider();
        async move { provider?.generate(req, sink).await }
    }

    fn embed(
        &self,
        req: EmbedRequest,
    ) -> impl Future<Output = Result<EmbedResponse, ProviderError>> + Send {
        let provider = self.provider();
        async move { provider?.embed(req).await }
    }

    fn count_tokens(
        &self,
        req: GenerateRequest,
    ) -> impl Future<Output = Result<u32, ProviderError>> + Send {
        let provider = self.provider();
        async move { provider?.count_tokens(req).await }
    }
}
