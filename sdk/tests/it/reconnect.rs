//! `run`: обрыв → реконнект с `hello.resume` и продолжением счётчика id, без
//! повторной регистрации surface; `shutdown` закрывает соединение.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{StreamExt as _, stream};
use tokio::sync::{mpsc, oneshot};

use eva::url::Url;
use eva_core::prelude::*;
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        ByteStream, Response,
        blobs::{get as blob_get, put as blob_put},
        surfaces::{get, update},
    },
    types::{BlobId, Bytes, ChatId, SurfaceId, blob},
    wire::{ErrorCode, RpcError, Welcome, frame::Payload},
};
use eva_rpc::{
    Call, Config, Kind, Listener, Outgoing, Service, codec,
    config::{Listen, UdsListen},
};
use eva_sdk::{Kernel, Module};

use crate::fake::{self, Surfaces};

async fn listen(dir: &tempfile::TempDir) -> (Listener, Url) {
    let path = dir.path().join("modules.sock");
    let listener = Listener::bind(
        &Listen::Uds(UdsListen { path: path.clone() }),
        &Config::default(),
    )
    .await
    .unwrap();
    let url = Url::parse(&format!("uds://{}", path.display())).unwrap();
    (listener, url)
}

#[tokio::test]
async fn reconnects_with_resume_and_registers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, url) = listen(&dir).await;
    let surfaces = Surfaces::default();
    let (sessions_tx, mut sessions) = mpsc::unbounded_channel();

    let kernel_surfaces = surfaces.clone();
    tokio::spawn(async move {
        for session in ["s1", "s2"] {
            let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
            let accepted =
                fake::accept(conn, fake::welcome(session, &[]), kernel_surfaces.clone()).await;
            if sessions_tx.send(accepted).is_err() {
                return;
            }
        }
        // Держим слушателя, пока тест не закончится.
        std::future::pending::<()>().await;
    });

    let connected = Module::new()
        .ask(true)
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
        .connect(&url, &fake::token("token"))
        .await
        .unwrap();
    let first = sessions.recv().await.unwrap();
    assert!(first.hello.resume.is_none());
    assert_eq!(
        first.hello.token.as_ref().map(|t| t.expose()),
        Some("token")
    );
    assert!(first.hello.capabilities.ask);

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = connected.clone();
    let running = tokio::spawn(connected.run(async {
        _ = stopped.await;
    }));

    first
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "kernel restarts"));
    let second = tokio::time::timeout(Duration::from_secs(5), sessions.recv())
        .await
        .expect("the module reconnects")
        .unwrap();
    let resume = second.hello.resume.as_ref().expect("resume on reconnect");
    assert_eq!(resume.session_id.as_str(), "s1");
    assert!(
        second.hello_id > first.hello_id,
        "ids continue: {}",
        second.hello_id
    );
    assert!(second.hello.capabilities.ask, "ask rides every hello");
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.welcome().session_id.as_str() != "s2" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the handle follows the new connection");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        surfaces.calls().is_empty(),
        "serving is derived: nothing to re-register"
    );

    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), second.peer.closed())
        .await
        .expect("shutdown closes the connection");
}

/// Ядро, которое записывает вызов и не отвечает: обрыв застаёт его в полёте.
#[derive(Clone, Default)]
pub struct Silent {
    pub calls: Arc<Mutex<Vec<(u64, String)>>>,
}

impl Silent {
    pub async fn wait_calls(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.calls.lock().unwrap().len() < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("calls reach the silent kernel");
    }
}

impl Service for Silent {
    fn kind(&self, method: &str) -> Option<Kind> {
        match method {
            "blobs.get" => Some(Kind::Stream),
            "blobs.put" => Some(Kind::Upload),
            _ => method.starts_with("surfaces.").then_some(Kind::Unary),
        }
    }

    async fn unary(&self, call: Call) -> Result<Payload, RpcError> {
        self.calls.lock().unwrap().push((call.id, call.method));
        std::future::pending().await
    }

    async fn stream(&self, call: Call) -> Result<Outgoing, RpcError> {
        self.calls.lock().unwrap().push((call.id, call.method));
        std::future::pending().await
    }

    async fn upload(&self, call: Call, body: ByteStream) -> Result<Payload, RpcError> {
        self.calls.lock().unwrap().push((call.id, call.method));
        let _body = body;
        std::future::pending().await
    }
}

/// `blobs.get` отдаёт `Meta` и чанк `hi`; с `hang` после `Meta` молчит.
#[derive(Clone, Default)]
struct Blobs {
    calls: Arc<Mutex<Vec<String>>>,
    hang: bool,
}

impl Blobs {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Service for Blobs {
    fn kind(&self, method: &str) -> Option<Kind> {
        match method {
            "blobs.get" => Some(Kind::Stream),
            "blobs.put" => Some(Kind::Upload),
            _ => None,
        }
    }

    async fn unary(&self, call: Call) -> Result<Payload, RpcError> {
        Err(RpcError::unknown_method(&call.method))
    }

    async fn stream(&self, call: Call) -> Result<Outgoing, RpcError> {
        self.calls.lock().unwrap().push(call.method);
        let meta = codec::encode(&blob_get::Item::Meta {
            name: "a.txt".to_owned(),
            media_type: "text/plain".to_owned(),
            size: 2,
        })
        .unwrap();
        let chunk = codec::encode(&blob_get::Item::Chunk(Bytes(b"hi".to_vec()))).unwrap();
        let head = stream::iter([Ok(meta)]);
        Ok(if self.hang {
            Box::pin(head.chain(stream::pending()))
        } else {
            Box::pin(head.chain(stream::iter([Ok(chunk)])))
        })
    }

    async fn upload(&self, call: Call, body: ByteStream) -> Result<Payload, RpcError> {
        self.calls.lock().unwrap().push(call.method.clone());
        drop(body);
        Err(RpcError::unknown_method(&call.method))
    }
}

/// Модуль с `run` и ручкой `Kernel` над ядром из двух сессий: `s1` над
/// `first`, после обрыва — `second` со своим `welcome`.
struct Outage {
    kernel: Kernel,
    first: fake::Session,
    sessions: mpsc::UnboundedReceiver<fake::Session>,
    stop: oneshot::Sender<()>,
    running: tokio::task::JoinHandle<eyre::Result<()>>,
    _dir: tempfile::TempDir,
}

impl Outage {
    async fn start(first: impl Service, second: impl Service, welcome: Welcome) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (listener, url) = listen(&dir).await;
        let (sessions_tx, mut sessions) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
            _ = sessions_tx.send(fake::accept(conn, fake::welcome("s1", &[]), first).await);
            let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
            _ = sessions_tx.send(fake::accept(conn, welcome, second).await);
            std::future::pending::<()>().await;
        });

        let connected = Module::new()
            .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
            .connect(&url, &fake::token("token"))
            .await
            .unwrap();
        let kernel = connected.kernel();
        let (stop, stopped) = oneshot::channel::<()>();
        let running = tokio::spawn(connected.run(async {
            _ = stopped.await;
        }));
        let first = sessions.recv().await.unwrap();
        Self {
            kernel,
            first,
            sessions,
            stop,
            running,
            _dir: dir,
        }
    }

    /// Ядро рвёт `s1` с `reason`; модуль переподключается.
    async fn cut(&mut self, reason: RpcError) -> fake::Session {
        self.first.peer.close(reason);
        tokio::time::timeout(Duration::from_secs(5), self.sessions.recv())
            .await
            .expect("the module reconnects")
            .unwrap()
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        self.running.await.unwrap().unwrap();
    }
}

pub async fn get(mut kernel: Kernel) -> Response<get::Ok, get::Err> {
    kernel
        .surfaces()
        .get()
        .call(get::Args {
            surface: SurfaceId::from("x"),
        })
        .await
}

pub async fn update(mut kernel: Kernel) -> Response<update::Ok, update::Err> {
    kernel
        .surfaces()
        .update()
        .call(update::Args {
            surface: SurfaceId::from("x"),
            update: update::Update::default(),
        })
        .await
}

/// id запросов метода `method` в порядке прихода.
pub fn id_of(requests: &[(u64, String)], method: &str) -> Vec<u64> {
    requests
        .iter()
        .filter(|(_, m)| m == method)
        .map(|(id, _)| *id)
        .collect()
}

/// `surfaces.*` поддельного ядра с одной строкой `x`.
pub fn surface_x(name: &str) -> Surfaces {
    let surfaces = Surfaces::default();
    surfaces.rows.lock().unwrap().insert(
        SurfaceId::from("x"),
        Surfaces::row(SurfaceId::from("x"), name.to_owned(), None),
    );
    surfaces
}

/// Ручка `Kernel`, взятая до обрыва, ведёт в новую сессию; оборванные чтение
/// и мутация повторяются там, мутация — с тем же id для окна `hello.resume`.
#[tokio::test]
async fn a_kept_kernel_follows_the_reconnect_and_resends_in_flight_calls() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, url) = listen(&dir).await;
    let silent = Silent::default();
    let surfaces = surface_x("X");
    let (sessions_tx, mut sessions) = mpsc::unbounded_channel();

    let (first_kernel, second_kernel) = (silent.clone(), surfaces.clone());
    tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
        let first = fake::accept(conn, fake::welcome("s1", &[]), first_kernel).await;
        _ = sessions_tx.send(first);
        let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
        let second = fake::accept(conn, fake::resumed("s2"), second_kernel).await;
        _ = sessions_tx.send(second);
        std::future::pending::<()>().await;
    });

    let connected = Module::new()
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
        .connect(&url, &fake::token("token"))
        .await
        .unwrap();
    let kernel = connected.kernel();
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(connected.run(async {
        _ = stopped.await;
    }));
    let first = sessions.recv().await.unwrap();

    let read = tokio::spawn(get(kernel.clone()));
    let write = tokio::spawn(update(kernel.clone()));
    silent.wait_calls(2).await;

    first
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "kernel restarts"));
    let _second = tokio::time::timeout(Duration::from_secs(5), sessions.recv())
        .await
        .expect("the module reconnects")
        .unwrap();

    let read = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the read is resent")
        .unwrap();
    assert_eq!(read.expect("served by the new session").surface.name, "X");
    let write = tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the write is resent")
        .unwrap();
    write.expect("the resent write is answered");

    let cut = silent.calls.lock().unwrap().clone();
    let resent = surfaces.requests.lock().unwrap().clone();
    assert_eq!(
        id_of(&resent, "surfaces.update"),
        id_of(&cut, "surfaces.update"),
        "the mutation keeps its request id"
    );
    assert_eq!(id_of(&resent, "surfaces.get").len(), 1);

    update(kernel)
        .await
        .expect("the kept handle mutates on the new session");
    let requests = surfaces.requests.lock().unwrap().clone();
    let updates = id_of(&requests, "surfaces.update");
    assert_eq!(updates.len(), 2);
    assert!(
        updates[1] > updates[0],
        "a new mutation takes a fresh id: {updates:?}"
    );

    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Ядро не продолжило окно (рестарт): оборванная мутация не повторяется —
/// тот же id в новом окне исполнил бы её второй раз, а исход первой неизвестен.
#[tokio::test]
async fn a_mutation_cut_by_a_kernel_restart_is_not_resent() {
    let silent = Silent::default();
    let surfaces = surface_x("X");
    let mut outage =
        Outage::start(silent.clone(), surfaces.clone(), fake::welcome("s2", &[])).await;

    let read = tokio::spawn(get(outage.kernel.clone()));
    let write = tokio::spawn(update(outage.kernel.clone()));
    silent.wait_calls(2).await;
    let second = outage
        .cut(RpcError::new(ErrorCode::Unavailable, "kernel restarts"))
        .await;
    assert!(second.hello.resume.is_some(), "the module asks to resume");

    let write = tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the cut write ends")
        .unwrap();
    assert!(
        matches!(&write, Err(Generic::Aux(Aux::Unavailable(m))) if m.contains("outcome unknown")),
        "{write:?}"
    );
    let read = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the read is resent")
        .unwrap();
    assert_eq!(read.expect("reads survive any reconnect").surface.name, "X");

    let cut = silent.calls.lock().unwrap().clone();
    let resent = surfaces.requests.lock().unwrap().clone();
    assert_eq!(id_of(&cut, "surfaces.update").len(), 1);
    assert!(id_of(&resent, "surfaces.update").is_empty(), "{resent:?}");

    update(outage.kernel.clone())
        .await
        .expect("a new mutation goes to the new session");
    outage.stop().await;
}

const BACKLOG: &str = "control backlog exceeded: the peer does not read";

/// Причина закрытия не из класса `unavailable`: вызывающий всё равно видит `Aux::Unavailable`.
fn backlog() -> RpcError {
    RpcError::new(ErrorCode::Quota, BACKLOG)
}

fn blob_args() -> blob_get::Args {
    blob_get::Args {
        id: BlobId::from("b1"),
        offset: 0,
    }
}

async fn read_blob(
    mut kernel: Kernel,
) -> Response<Vec<Response<blob_get::Item, blob_get::Err>>, blob_get::Err> {
    let items = kernel.blobs().get().call(blob_args()).await?;
    Ok(items.collect().await)
}

async fn put_blob(mut kernel: Kernel) -> Response<blob_put::Ok, blob_put::Err> {
    let body: ByteStream = Box::pin(stream::iter([Ok(eva::bytes::Bytes::from_static(b"hi"))]));
    kernel
        .blobs()
        .put()
        .call(blob_put::Args {
            meta: blob_put::Meta {
                chat: ChatId(1),
                name: "a.txt".to_owned(),
                media_type: "text/plain".to_owned(),
                kind: blob::Kind::File,
                size: 2,
                hash: None,
            },
            body,
        })
        .await
}

/// Открытие потока, оборванное до первого элемента, повторяется в новой
/// сессии: поток — чтение, продолжать окно для него не нужно. Многопоточный
/// рантайм: конец потока и `close` разбирает не та задача, что ждёт открытия.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_stream_open_is_reopened_on_the_new_session() {
    let silent = Silent::default();
    let blobs = Blobs::default();
    let mut outage = Outage::start(silent.clone(), blobs.clone(), fake::welcome("s2", &[])).await;

    let read = tokio::spawn(read_blob(outage.kernel.clone()));
    silent.wait_calls(1).await;
    outage
        .cut(RpcError::new(ErrorCode::Unavailable, "kernel restarts"))
        .await;

    let items = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the open is resent")
        .unwrap()
        .expect("opened on the new session");
    assert!(
        matches!(
            items.as_slice(),
            [Ok(blob_get::Item::Meta { .. }), Ok(blob_get::Item::Chunk(chunk))] if chunk.0 == b"hi"
        ),
        "{items:?}"
    );
    assert_eq!(silent.calls.lock().unwrap().len(), 1);
    assert_eq!(blobs.calls(), ["blobs.get"]);
    outage.stop().await;
}

/// Начатый поток не переоткрывается: хвост кончается `Aux::Unavailable` при
/// любой причине закрытия (`11 §6`).
#[tokio::test]
async fn a_stream_cut_after_its_first_item_ends_unavailable() {
    let first = Blobs {
        hang: true,
        ..Blobs::default()
    };
    let second = Blobs::default();
    let mut outage = Outage::start(first.clone(), second.clone(), fake::resumed("s2")).await;

    let mut kernel = outage.kernel.clone();
    let mut items = kernel
        .blobs()
        .get()
        .call(blob_args())
        .await
        .expect("opened");
    assert!(matches!(
        items.next().await,
        Some(Ok(blob_get::Item::Meta { .. }))
    ));
    outage.cut(backlog()).await;

    let tail = tokio::time::timeout(Duration::from_secs(5), items.collect::<Vec<_>>())
        .await
        .expect("the cut stream ends");
    assert!(
        matches!(tail.as_slice(), [Err(Generic::Aux(Aux::Unavailable(m)))] if m == BACKLOG),
        "{tail:?}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(first.calls(), ["blobs.get"]);
    assert!(second.calls().is_empty(), "{:?}", second.calls());
    outage.stop().await;
}

/// Тело загрузки уже прочитано: оборванная загрузка отвечает
/// `Aux::Unavailable` и в новую сессию не уходит.
#[tokio::test]
async fn a_cut_upload_is_unavailable_and_not_resent() {
    let silent = Silent::default();
    let blobs = Blobs::default();
    let mut outage = Outage::start(silent.clone(), blobs.clone(), fake::resumed("s2")).await;

    let upload = tokio::spawn(put_blob(outage.kernel.clone()));
    silent.wait_calls(1).await;
    outage.cut(backlog()).await;

    let upload = tokio::time::timeout(Duration::from_secs(5), upload)
        .await
        .expect("the cut upload ends")
        .unwrap();
    assert!(
        matches!(&upload, Err(Generic::Aux(Aux::Unavailable(m))) if m == BACKLOG),
        "{upload:?}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(blobs.calls().is_empty(), "{:?}", blobs.calls());
    outage.stop().await;
}

/// Вытеснение рвёт вызов в полёте: вызывающий видит `Aux::Unavailable`, а не
/// `conflict` закрытия (`11 §6`); после конца `run` ручка реконнекта не ждёт.
#[tokio::test]
async fn a_call_cut_by_supersede_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, url) = listen(&dir).await;
    let silent = Silent::default();
    let (session_tx, session_rx) = oneshot::channel();
    let kernel_side = silent.clone();
    tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
        _ = session_tx.send(fake::accept(conn, fake::welcome("s1", &[]), kernel_side).await);
        std::future::pending::<()>().await;
    });

    let connected = Module::new()
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
        .connect(&url, &fake::token("token"))
        .await
        .unwrap();
    let kernel = connected.kernel();
    let running = tokio::spawn(connected.run(std::future::pending()));
    let session = session_rx.await.unwrap();

    let read = tokio::spawn(get(kernel.clone()));
    silent.wait_calls(1).await;
    session
        .peer
        .close(RpcError::new(ErrorCode::Conflict, "superseded"));

    let ended = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("run stops on supersede")
        .unwrap();
    assert!(ended.is_err());
    let read = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the cut read ends")
        .unwrap();
    assert!(
        matches!(&read, Err(Generic::Aux(Aux::Unavailable(m))) if m == "superseded"),
        "{read:?}"
    );

    let after = tokio::time::timeout(Duration::from_secs(1), get(kernel))
        .await
        .expect("no reconnect to wait for");
    assert!(
        matches!(after, Err(Generic::Aux(Aux::Unavailable(_)))),
        "{after:?}"
    );
}

/// Мутация, начатая, пока ядра нет, ждёт реконнекта и уходит в новую сессию
/// один раз — она не была в полёте, повторять нечего.
#[tokio::test]
async fn a_call_during_the_outage_waits_for_the_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, url) = listen(&dir).await;
    let surfaces = surface_x("X");
    let (sessions_tx, mut sessions) = mpsc::unbounded_channel();
    let (reopen, reopened) = oneshot::channel::<()>();

    let kernel_side = surfaces.clone();
    tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
        _ = sessions_tx.send(fake::accept(conn, fake::welcome("s1", &[]), Silent::default()).await);
        _ = reopened.await;
        let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
        _ = sessions_tx.send(fake::accept(conn, fake::welcome("s2", &[]), kernel_side).await);
        std::future::pending::<()>().await;
    });

    let connected = Module::new()
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
        .connect(&url, &fake::token("token"))
        .await
        .unwrap();
    let kernel = connected.kernel();
    let old = connected.client();
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(connected.run(async {
        _ = stopped.await;
    }));
    let first = sessions.recv().await.unwrap();
    first
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "kernel restarts"));
    tokio::time::timeout(Duration::from_secs(5), old.closed())
        .await
        .expect("the module sees the close");
    drop(old);

    let write = tokio::spawn(update(kernel));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!write.is_finished(), "the call waits for the kernel");

    reopen.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the call proceeds after the reconnect")
        .unwrap()
        .expect("served by the new session");
    assert_eq!(surfaces.calls(), ["surfaces.update"]);

    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn connect_waits_for_the_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modules.sock");
    let url = Url::parse(&format!("uds://{}", path.display())).unwrap();

    let module = Module::new()
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(20))
        .connect_wait(Duration::from_secs(5));
    let started = tokio::spawn(async move { module.connect(&url, &fake::token("token")).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (listener, _) = listen(&dir).await;
    let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
    let kernel = fake::accept(conn, fake::welcome("s1", &[]), Surfaces::default()).await;
    let connected = started
        .await
        .unwrap()
        .expect("connects once the kernel is up");
    assert_eq!(connected.welcome().session_id.as_str(), "s1");
    drop(kernel);
}

#[tokio::test]
async fn gives_up_after_connect_wait() {
    let dir = tempfile::tempdir().unwrap();
    let url = Url::parse(&format!("uds://{}", dir.path().join("none.sock").display())).unwrap();
    let res = Module::new()
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(20))
        .connect_wait(Duration::from_millis(100))
        .connect(&url, &fake::token("token"))
        .await;
    assert!(matches!(res, Err(eva_sdk::ConnectError::Transport(_))));
}

#[tokio::test]
async fn rejected_hello_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, url) = listen(&dir).await;
    tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap().upgrade().await.unwrap();
        let raw = conn.recv().await.unwrap().unwrap();
        let eva_protocol::wire::Frame::Request(request) =
            eva_rpc::codec::decode_frame(&raw).unwrap()
        else {
            panic!("hello expected");
        };
        let reply = eva_protocol::wire::Frame::Response(eva_protocol::wire::frame::Response::err(
            request.id,
            RpcError::new(ErrorCode::Unauthorized, "bad token"),
        ));
        conn.send(eva_rpc::transport::Outbound::Frame(
            eva_rpc::codec::encode_frame(&reply).unwrap(),
        ))
        .await
        .unwrap();
        std::future::pending::<()>().await;
    });

    let res = Module::new()
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(20))
        .connect(&url, &fake::token("wrong"))
        .await;
    match res {
        Err(eva_sdk::ConnectError::Rejected(e)) => assert_eq!(e.code, ErrorCode::Unauthorized),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

pub mod tools {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use futures::StreamExt as _;
    use serde_json::json;
    use tokio::sync::Notify;

    use eva_protocol::{
        types::ChatId,
        wire::tools::{ToolCall, ToolItem, ToolResult},
    };
    use eva_sdk::{ToolContent, ToolCtx, ToolError};

    use super::*;
    use crate::dispatch::EchoInput;

    pub struct Counters {
        pub started: AtomicUsize,
        pub finished: AtomicUsize,
        pub gate: Notify,
    }

    pub fn slow(counters: Arc<Counters>) -> Module {
        Module::new()
            .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
            .tool("x.slow", move |_: ToolCtx, _: EchoInput| {
                let counters = counters.clone();
                async move {
                    counters.started.fetch_add(1, Ordering::SeqCst);
                    counters.gate.notified().await;
                    counters.finished.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ToolError>(ToolResult::ok(vec![ToolContent::text("done")]))
                }
            })
    }

    pub fn call() -> ToolCall {
        let mut ctx = fake::turn();
        ctx.chat = ChatId(9);
        ToolCall {
            call_id: "toolu_1".to_owned(),
            tool_id: None,
            canonical: "x_slow".to_owned(),
            input: json!({"text": "", "times": 0}),
            ctx,
        }
    }

    pub async fn wait(what: &str, check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// Обрыв посреди `tools.call`: тул доживает, повтор с тем же `call_id`
    /// после `hello.resume` получает тот же результат, исполнение одно (`06 §12`).
    #[tokio::test]
    async fn call_survives_a_reconnect_and_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, url) = listen(&dir).await;
        let (sessions_tx, mut sessions) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for session in ["s1", "s2"] {
                let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
                let accepted =
                    fake::accept(conn, fake::welcome(session, &[]), Surfaces::default()).await;
                if sessions_tx.send(accepted).is_err() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        });

        let counters = Arc::new(Counters {
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            gate: Notify::new(),
        });
        let connected = slow(counters.clone())
            .connect(&url, &fake::token("token"))
            .await
            .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let running = tokio::spawn(connected.run(async {
            _ = stopped.await;
        }));

        let first = sessions.recv().await.unwrap();
        let module = first.module();
        let pending = tokio::spawn(async move {
            let items: Vec<_> = module.call_tool(&call()).await?.collect().await;
            Ok::<_, RpcError>(items)
        });
        wait("the tool to start", || {
            counters.started.load(Ordering::SeqCst) == 1
        })
        .await;
        first
            .peer
            .close(RpcError::new(ErrorCode::Unavailable, "socket lost"));
        _ = pending.await;

        let second = tokio::time::timeout(Duration::from_secs(5), sessions.recv())
            .await
            .expect("reconnect")
            .unwrap();
        assert!(second.hello.resume.is_some());
        let module = second.module();
        let retry = tokio::spawn(async move {
            let items: Vec<_> = module.call_tool(&call()).await.unwrap().collect().await;
            items
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        counters.gate.notify_waiters();
        let items = retry.await.unwrap();
        match items.last() {
            Some(Ok(ToolItem::Result(result))) => {
                assert_eq!(result.content, vec![ToolContent::text("done")]);
            }
            other => panic!("expected the result, got {other:?}"),
        }
        assert_eq!(counters.started.load(Ordering::SeqCst), 1);
        assert_eq!(counters.finished.load(Ordering::SeqCst), 1);

        stop.send(()).unwrap();
        running.await.unwrap().unwrap();
    }

    /// Тул, переживший обрыв, ходит в ядро через новую сессию: `ToolCtx::kernel`
    /// не привязан к соединению, по которому пришёл вызов.
    #[tokio::test]
    async fn a_tool_reaches_the_kernel_through_the_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, url) = listen(&dir).await;
        let (sessions_tx, mut sessions) = mpsc::unbounded_channel();
        let second = surface_x("X2");
        let kernels = [Surfaces::default(), second.clone()];
        tokio::spawn(async move {
            for (session, surfaces) in ["s1", "s2"].into_iter().zip(kernels) {
                let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
                let accepted = fake::accept(conn, fake::welcome(session, &[]), surfaces).await;
                if sessions_tx.send(accepted).is_err() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        });

        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        let module = Module::new()
            .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
            .tool("x.slow", {
                let (started, gate) = (started.clone(), gate.clone());
                move |ctx: ToolCtx, _: EchoInput| {
                    let (started, gate) = (started.clone(), gate.clone());
                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        gate.notified().await;
                        let name = get(ctx.kernel()).await?.surface.name;
                        Ok::<_, ToolError>(ToolResult::ok(vec![ToolContent::text(name)]))
                    }
                }
            });
        let connected = module.connect(&url, &fake::token("token")).await.unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let running = tokio::spawn(connected.run(async {
            _ = stopped.await;
        }));

        let first = sessions.recv().await.unwrap();
        let module = first.module();
        let pending = tokio::spawn(async move { module.call_tool(&call()).await });
        wait("the tool to start", || started.load(Ordering::SeqCst) == 1).await;
        first
            .peer
            .close(RpcError::new(ErrorCode::Unavailable, "socket lost"));
        _ = pending.await;

        let second_session = tokio::time::timeout(Duration::from_secs(5), sessions.recv())
            .await
            .expect("reconnect")
            .unwrap();
        gate.notify_one();
        let items: Vec<_> = second_session
            .module()
            .call_tool(&call())
            .await
            .unwrap()
            .collect()
            .await;
        match items.last() {
            Some(Ok(ToolItem::Result(result))) => {
                assert_eq!(result.content, vec![ToolContent::text("X2")]);
            }
            other => panic!("expected the result, got {other:?}"),
        }
        assert_eq!(second.calls(), ["surfaces.get"]);
        assert_eq!(started.load(Ordering::SeqCst), 1);

        stop.send(()).unwrap();
        running.await.unwrap().unwrap();
    }

    /// Отмена ядром на живом соединении прерывает тул.
    #[tokio::test]
    async fn cancel_on_a_live_connection_aborts_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, url) = listen(&dir).await;
        let (session_tx, session_rx) = oneshot::channel();
        tokio::spawn(async move {
            let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
            let accepted = fake::accept(conn, fake::welcome("s1", &[]), Surfaces::default()).await;
            _ = session_tx.send(accepted);
            std::future::pending::<()>().await;
        });

        let counters = Arc::new(Counters {
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            gate: Notify::new(),
        });
        let _connected = slow(counters.clone())
            .connect(&url, &fake::token("token"))
            .await
            .unwrap();
        let session = session_rx.await.unwrap();

        let stream = session.module().call_tool(&call()).await.unwrap();
        wait("the tool to start", || {
            counters.started.load(Ordering::SeqCst) == 1
        })
        .await;
        drop(stream);
        tokio::time::sleep(Duration::from_millis(200)).await;
        counters.gate.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            counters.finished.load(Ordering::SeqCst),
            0,
            "cancel aborts the tool"
        );

        // Отменённый вызов не кэшируется: повтор исполняется заново.
        let items: Vec<_> = {
            let module = session.module();
            let retry = tokio::spawn(async move {
                module
                    .call_tool(&call())
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
            });
            wait("the retry to start", || {
                counters.started.load(Ordering::SeqCst) == 2
            })
            .await;
            counters.gate.notify_waiters();
            retry.await.unwrap()
        };
        assert!(matches!(items.last(), Some(Ok(ToolItem::Result(_)))));
        assert_eq!(counters.finished.load(Ordering::SeqCst), 1);
    }
}
