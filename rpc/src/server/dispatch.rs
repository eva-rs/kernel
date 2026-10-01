//! Диспетчер модуль → ядро (`11 §6`): метод `<ns>.<op>` → шаг сессии,
//! `params` → `Args`, `Ok` → `result`, `Generic` → `RpcError`.

use std::{collections::HashMap, fmt, sync::Arc};

use fastrace::{Span, future::FutureExt as _, prelude::SpanContext};
use futures::{StreamExt as _, future::BoxFuture};
use serde::{Serialize, de::DeserializeOwned};

use eva_core::{CallStep, Caller, IsSession, OpKind, Session, SessionMaker, service::op_name};
use eva_protocol::{
    errors::HasCode,
    requests::{ByteStream, Items, Response},
    wire::{ErrorCode, RpcError, frame::Payload},
};

use crate::{
    codec,
    peer::{Call, Kind, Outgoing, Service},
    server::window::{self, Begin, Lease},
};

type Unary<S> = fn(Session<S>, Payload) -> BoxFuture<'static, Result<Payload, RpcError>>;
type Stream<S> = fn(Session<S>, Payload) -> BoxFuture<'static, Result<Outgoing, RpcError>>;
type Upload<S> =
    fn(Session<S>, Payload, ByteStream) -> BoxFuture<'static, Result<Payload, RpcError>>;

enum Run<S> {
    Unary(Unary<S>),
    Stream(Stream<S>),
    Upload(Upload<S>),
}

struct Route<S> {
    kind: OpKind,
    run: Run<S>,
}

/// Маршруты всех операций; строится один раз на сервер.
pub(crate) struct Table<S> {
    routes: Vec<(String, Route<S>)>,
    index: HashMap<String, usize>,
}

impl<S: IsSession + 'static> Table<S> {
    pub fn new() -> Self {
        let routes = routes::<S>();
        let index = routes
            .iter()
            .enumerate()
            .map(|(i, (method, _))| (method.clone(), i))
            .collect();
        Self { routes, index }
    }
}

impl<S> Table<S> {
    fn get(&self, method: &str) -> Option<&Route<S>> {
        self.index.get(method).map(|&i| &self.routes[i].1)
    }

    fn methods(&self) -> Vec<(String, OpKind)> {
        self.routes
            .iter()
            .map(|(method, route)| (method.clone(), route.kind))
            .collect()
    }
}

/// Маршрут по флагу операции: `mutating` идёт через окно идемпотентности,
/// `stream` отдаёт `Items` кадрами потока, у `upload` по проводу `Meta`, а
/// `Args` собирается из неё и чанков тела.
macro_rules! route {
    ([] $accessor:ident $method:ident $Args:ty) => {
        route!(@unary Read $accessor $method)
    };
    ([mutating] $accessor:ident $method:ident $Args:ty) => {
        route!(@unary Mutating $accessor $method)
    };
    (@unary $kind:ident $accessor:ident $method:ident) => {
        Route {
            kind: OpKind::$kind,
            run: Run::Unary(|mut session, params| {
                Box::pin(async move {
                    let result = unary(session.$accessor().$method(), params).await;
                    result
                })
            }),
        }
    };
    ([stream] $accessor:ident $method:ident $Args:ty) => {
        Route {
            kind: OpKind::Stream,
            run: Run::Stream(|mut session, params| {
                Box::pin(async move {
                    let result = stream(session.$accessor().$method(), params).await;
                    result
                })
            }),
        }
    };
    ([upload] $accessor:ident $method:ident $Args:ty) => {
        Route {
            kind: OpKind::Upload,
            run: Run::Upload(|mut session, params, body| {
                Box::pin(async move {
                    type Args = $Args;

                    let args = |meta| Args { meta, body };
                    let result = upload(session.$accessor().$method(), params, args).await;
                    result
                })
            }),
        }
    };
}

macro_rules! routes {
    ($(
        $segment:ident {$(
            $accessor:ident : $Trait:ident {$(
                $(#[$flag:ident])*
                $method:ident = $endpoint:ident {
                    args: $Args:ty,
                    ok: $Ok:ty,
                    err: $Err:ty,
                    output: $Output:ty,
                    docs: [$($doc:literal),*],
                }
            )*}
        )*}
    )*) => {
        fn routes<S: IsSession + 'static>() -> Vec<(String, Route<S>)> {
            $($(use eva_core::service::$segment::$Trait as _;)*)*

            vec![$($($(
                (
                    format!("{}.{}", stringify!($segment), op_name(stringify!($endpoint))),
                    route!([$($flag)*] $accessor $method $Args),
                ),
            )*)*)*]
        }
    };
}

eva_core::for_each_op!(routes);

async fn unary<A, O, E>(
    mut step: impl CallStep<A, Output = Response<O, E>>,
    params: Payload,
) -> Result<Payload, RpcError>
where
    A: DeserializeOwned,
    O: Serialize,
    E: Serialize + HasCode + fmt::Display,
{
    let args = codec::decode(&params).map_err(codec::bad_params)?;
    match step.call(args).await {
        Ok(ok) => encode_ok(&ok),
        Err(error) => Err(codec::typed(&error)),
    }
}

async fn stream<A, T, E>(
    mut step: impl CallStep<A, Output = Response<Items<T, E>, E>>,
    params: Payload,
) -> Result<Outgoing, RpcError>
where
    A: DeserializeOwned,
    T: Serialize + 'static,
    E: Serialize + HasCode + fmt::Display + 'static,
{
    let args = codec::decode(&params).map_err(codec::bad_params)?;
    let items = step.call(args).await.map_err(|e| codec::typed(&e))?;
    Ok(Box::pin(items.map(|item| match item {
        Ok(item) => encode_ok(&item),
        Err(error) => Err(codec::typed(&error)),
    })))
}

async fn upload<M, A, O, E>(
    mut step: impl CallStep<A, Output = Response<O, E>>,
    params: Payload,
    args: impl FnOnce(M) -> A,
) -> Result<Payload, RpcError>
where
    M: DeserializeOwned,
    O: Serialize,
    E: Serialize + HasCode + fmt::Display,
{
    let meta = codec::decode(&params).map_err(codec::bad_params)?;
    match step.call(args(meta)).await {
        Ok(ok) => encode_ok(&ok),
        Err(error) => Err(codec::typed(&error)),
    }
}

fn encode_ok<T: Serialize>(ok: &T) -> Result<Payload, RpcError> {
    codec::encode(ok).map_err(|e| RpcError::new(ErrorCode::Internal, e))
}

/// Обработчик запросов одной сессии модуля: `make_session(caller)` на каждый запрос.
pub struct Dispatcher<M: SessionMaker> {
    maker: Arc<M>,
    caller: Caller,
    table: Arc<Table<M::Session>>,
    window: Lease,
}

impl<M: SessionMaker> Dispatcher<M>
where
    M::Session: 'static,
{
    pub(crate) fn new(
        maker: Arc<M>,
        caller: Caller,
        table: Arc<Table<M::Session>>,
        window: Lease,
    ) -> Self {
        Self {
            maker,
            caller,
            table,
            window,
        }
    }

    /// Все RPC-методы с формой шага, в порядке таблицы операций.
    pub fn methods() -> Vec<(String, OpKind)> {
        Table::<M::Session>::new().methods()
    }

    async fn session(&self) -> Result<Session<M::Session>, RpcError> {
        self.maker
            .make_session(self.caller.clone())
            .await
            .map_err(|aux| codec::aux(&aux))
    }

    async fn run(&self, run: Unary<M::Session>, params: Payload) -> Result<Payload, RpcError> {
        run(self.session().await?, params).await
    }

    /// Корень трассы запроса: задачи соединения не наследуют спан
    /// компонента, а `#[fastrace::trace]` хендлеров без родителя молчит.
    fn span(&self, method: &str) -> Span {
        let caller = self.caller.label().to_owned();
        Span::root(method.to_owned(), SpanContext::random()).with_property(|| ("caller", caller))
    }

    /// Край логирует ошибку один раз: хендлер операции её не логирует.
    fn logged<T>(&self, method: &str, result: Result<T, RpcError>) -> Result<T, RpcError> {
        if let Err(e) = &result {
            let caller = self.caller.label();
            match e.code {
                ErrorCode::Forbidden => {
                    log::debug!(method, caller, error:% = e; "operation forbidden")
                }
                ErrorCode::Internal => log::error!(method, caller, error:% = e; "operation failed"),
                _ => {}
            }
        }
        result
    }
}

fn unknown(method: &str) -> RpcError {
    RpcError::unknown_method(method)
}

impl<M> Service for Dispatcher<M>
where
    M: SessionMaker + 'static,
    M::Session: 'static,
{
    fn kind(&self, method: &str) -> Option<Kind> {
        self.table.get(method).map(|route| match route.run {
            Run::Unary(_) => Kind::Unary,
            Run::Stream(_) => Kind::Stream,
            Run::Upload(_) => Kind::Upload,
        })
    }

    fn survives_cancel(&self, method: &str) -> bool {
        self.table
            .get(method)
            .is_some_and(|route| route.kind == OpKind::Mutating)
    }

    async fn unary(&self, call: Call) -> Result<Payload, RpcError> {
        let Call { id, method, params } = call;
        let Some(&Route {
            kind,
            run: Run::Unary(run),
        }) = self.table.get(&method)
        else {
            return Err(unknown(&method));
        };

        if kind != OpKind::Mutating {
            let result = self.run(run, params).in_span(self.span(&method)).await;
            return self.logged(&method, result);
        }

        match self.window.begin(id, &method) {
            Begin::Run(slot) => {
                let result = self.run(run, params).in_span(self.span(&method)).await;
                slot.fill(&result);
                self.logged(&method, result)
            }
            Begin::Cached(result) => result,
            Begin::Wait(done) => window::wait(done).await,
            Begin::Reused => Err(RpcError::invalid_params(format_args!(
                "request id {id} already carried another method"
            ))),
        }
    }

    async fn stream(&self, call: Call) -> Result<Outgoing, RpcError> {
        let Call { method, params, .. } = call;
        let Some(&Route {
            run: Run::Stream(run),
            ..
        }) = self.table.get(&method)
        else {
            return Err(unknown(&method));
        };

        let result = async {
            match self.session().await {
                Ok(session) => run(session, params).await,
                Err(e) => Err(e),
            }
        }
        .in_span(self.span(&method))
        .await;
        self.logged(&method, result)
    }

    async fn upload(&self, call: Call, body: ByteStream) -> Result<Payload, RpcError> {
        let Call { method, params, .. } = call;
        let Some(&Route {
            run: Run::Upload(run),
            ..
        }) = self.table.get(&method)
        else {
            return Err(unknown(&method));
        };

        let result = async {
            match self.session().await {
                Ok(session) => run(session, params, body).await,
                Err(e) => Err(e),
            }
        }
        .in_span(self.span(&method))
        .await;
        self.logged(&method, result)
    }

    async fn notify(&self, method: String, _params: Payload) {
        log::debug!(method = method.as_str(), caller = self.caller.label(); "notification without a handler");
    }
}
