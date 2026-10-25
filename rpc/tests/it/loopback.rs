//! Операции библиотеки через RPC-клиент: каждый тест гоняется по uds и по ws.

use std::time::Duration;

use futures::{StreamExt as _, stream};

use eva::handling::CallStep as _;
use eva_core::{Caller, SessionMaker as _};
use eva_protocol::{
    errors::{self, Aux, Generic},
    requests::{
        Blobs::{Trait as _, put},
        ByteStream,
        Surfaces::{Trait as _, create, get, list},
        Turns::{Trait as _, attach},
    },
    types::blob::Kind as BlobKind,
    wire::{ErrorCode, RpcError, frame::Payload, method},
};
use eva_rpc::{ClientOptions, ConnectError, NoHandler, RpcClient, codec, config::Config};

use crate::{
    harness::{Dial, Harness, Via, config, hello, ms, service_hello},
    kernel::{CLIENT, CLIENT_TOKEN, FLOOD_ITEMS, SERVICE, chat, chat_id, item},
};

macro_rules! both {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            #[tokio::test(flavor = "multi_thread")]
            async fn uds() {
                super::$name(crate::harness::Via::Uds).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn ws() {
                super::$name(crate::harness::Via::Ws).await;
            }
        }
    )*};
}

both! {
    unary_ok,
    spec_error,
    aux_error,
    stream_items,
    cancel_on_drop,
    upload,
    unknown_method,
    bad_token,
    caller_from_welcome,
    backpressure_per_stream,
    max_frame_closes,
    supersede_ends_streams,
    request_timeout_keeps_mutation,
    heartbeat_keeps_long_call,
    upload_of_small_pieces,
    idle_stream_times_out,
    heartbeat_keeps_stream,
}

fn create_args(id: &str) -> create::Args {
    create::Args {
        id: id.into(),
        name: id.to_owned(),
        rules: None,
    }
}

async fn unary_ok(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;
    let mut session = client.session();

    let create::Ok { surface } = session
        .surfaces()
        .inner_mut()
        .create()
        .call(create_args("x"))
        .await
        .unwrap();
    assert_eq!(surface.id.as_str(), "x");

    let get::Ok { surface: got } = session.surfaces().get("x".into()).await.unwrap();
    assert_eq!(got, surface);
}

async fn spec_error(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;

    let err = client
        .session()
        .surfaces()
        .get("nope".into())
        .await
        .unwrap_err();
    let expected = errors::surfaces::NotFound {
        surface: "nope".into(),
    };
    assert_eq!(err, Generic::Spec(expected.into()));
}

async fn aux_error(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;

    let err = client
        .session()
        .surfaces()
        .inner_mut()
        .list()
        .call(list::Args::default())
        .await
        .unwrap_err();
    assert_eq!(err, Generic::Aux(Aux::Forbidden("owner only".to_owned())));
}

fn attach_args(chat: i64) -> attach::Args {
    attach::Args {
        chat: chat_id(chat),
        since_seq: None,
    }
}

async fn stream_items(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;
    let mut session = client.session();

    let items: Vec<_> = session
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::FINITE))
        .await
        .map(|ok| ok.0)
        .unwrap()
        .collect()
        .await;
    assert_eq!(items, (0..3).map(|n| Ok(item(n))).collect::<Vec<_>>());

    let not_found = || {
        Generic::Spec(attach::Err::from(errors::chats::NotFound {
            chat: chat_id(chat::BROKEN).into(),
        }))
    };
    let items: Vec<_> = session
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::BROKEN))
        .await
        .map(|ok| ok.0)
        .unwrap()
        .collect()
        .await;
    assert_eq!(items, vec![Ok(item(0)), Ok(item(1)), Err(not_found())]);

    let Err(err) = session
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::MISSING))
        .await
        .map(|ok| ok.0)
    else {
        panic!("open of a missing chat must fail before the first item");
    };
    assert_eq!(
        err,
        Generic::Spec(attach::Err::from(errors::chats::NotFound {
            chat: chat_id(chat::MISSING).into(),
        }))
    );
}

async fn cancel_on_drop(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;

    let mut items = client
        .session()
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::ENDLESS))
        .await
        .map(|ok| ok.0)
        .unwrap();
    assert_eq!(items.next().await, Some(Ok(item(0))));
    drop(items);

    tokio::time::timeout(
        Duration::from_secs(5),
        h.kernel.state.attach_dropped.notified(),
    )
    .await
    .expect("server stream was not dropped after cancel");
}

fn body(parts: Vec<Vec<u8>>) -> ByteStream {
    Box::pin(stream::iter(parts.into_iter().map(|p| Ok(p.into()))))
}

async fn upload(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;
    let mut session = client.session();

    // Больше `blob_chunk`: клиент режет на несколько `stream_item`.
    let big = (0..2_500_000_u32).map(|i| i as u8).collect::<Vec<_>>();
    let parts = vec![b"hello ".to_vec(), big.clone(), b"bye".to_vec()];
    let whole = parts.concat();

    let args = |size: u64, body: ByteStream| put::Args {
        meta: put::Meta {
            chat: chat_id(1),
            name: "file.bin".to_owned(),
            media_type: "application/octet-stream".to_owned(),
            kind: BlobKind::File,
            size,
            hash: None,
        },
        body,
    };

    let ok = session
        .blobs()
        .inner_mut()
        .put()
        .call(args(whole.len() as u64, body(parts.clone())))
        .await
        .unwrap();
    assert_eq!(ok.size, whole.len() as u64);
    assert_eq!(*h.kernel.state.uploads.lock().unwrap(), vec![whole.clone()]);

    let err = session
        .blobs()
        .inner_mut()
        .put()
        .call(args(1, body(parts)))
        .await
        .unwrap_err();
    let expected = errors::blobs::SizeMismatch {
        declared: 1,
        actual: whole.len() as u64,
    };
    assert_eq!(err, Generic::Spec(expected.into()));

    let failing: ByteStream = Box::pin(stream::iter([
        Ok(b"part".to_vec().into()),
        Err(Aux::Unavailable("disk gone".to_owned())),
    ]));
    let err = session
        .blobs()
        .inner_mut()
        .put()
        .call(args(4, failing))
        .await
        .unwrap_err();
    assert_eq!(err, Generic::Aux(Aux::Unavailable("disk gone".to_owned())));
}

async fn unknown_method(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;

    let empty: Payload = codec::encode(&serde_json::json!({})).unwrap();
    let err = client.peer().call("nope.op", empty).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownMethod);
    assert!(matches!(
        eva_rpc::client::from_rpc::<()>(err),
        Generic::Aux(Aux::Invalid(_))
    ));

    // Соединение живо после незнакомого метода.
    assert!(!client.peer().is_closed());
    let err = client
        .session()
        .surfaces()
        .inner_mut()
        .list()
        .call(list::Args::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Generic::Aux(Aux::Forbidden(_))));
}

async fn bad_token(via: Via) {
    let h = Harness::start(via).await;

    let rejected = |result: Result<(RpcClient, _), ConnectError>| match result {
        Err(ConnectError::Rejected(e)) => e,
        other => panic!("expected rejection, got {other:?}"),
    };

    let err =
        rejected(RpcClient::connect_with(&h.target, hello("stolen"), h.options(), NoHandler).await);
    assert_eq!(err.code, ErrorCode::Unauthorized);

    let mut old = hello(CLIENT_TOKEN);
    old.proto_version.major += 1;
    let err = rejected(RpcClient::connect_with(&h.target, old, h.options(), NoHandler).await);
    assert_eq!(err.code, ErrorCode::ProtoVersion);
}

async fn caller_from_welcome(via: Via) {
    let mut h = Harness::start(via).await;

    // Имени модуля в `hello` нет: id и caller клиента — из записи токена,
    // сервиса — из записи, которую набрало ядро.
    let (client, accepted) = h.connect(CLIENT_TOKEN).await;
    assert_eq!(client.caller(), &accepted.caller);
    assert!(client.caller().is_owner());
    assert_eq!(client.welcome().module.as_str(), CLIENT);
    assert_eq!(client.caller().module().map(|m| m.as_str()), Some(CLIENT));

    let dial = match via {
        Via::Uds => Dial::Uds,
        Via::Ws => Dial::Tcp,
    };
    let (service, accepted) = h.dial_with(dial, service_hello(), NoHandler).await;
    assert_eq!(service.caller(), &accepted.caller);
    assert!(!service.caller().is_owner());
    assert_eq!(accepted.welcome.module.as_str(), SERVICE);
    assert_eq!(service.caller().module().map(|m| m.as_str()), Some(SERVICE));

    // Токен решает, кто ты: чужого caller'а клиент не выдаст.
    let spoofed = service
        .make_session(client.caller().clone())
        .await
        .unwrap_err();
    assert!(matches!(spoofed, Aux::Forbidden(_)));
    assert!(matches!(
        service.make_session(Caller::Kernel).await,
        Err(Aux::Forbidden(_))
    ));
    assert!(service.make_session(service.caller().clone()).await.is_ok());
}

async fn backpressure_per_stream(via: Via) {
    let cfg = Config {
        ping_interval: ms(50),
        ping_timeout: ms(400),
        ..config()
    };
    let mut h = Harness::with(via, cfg).await;
    let (client, accepted) = h.connect(CLIENT_TOKEN).await;
    let mut session = client.session();

    session
        .surfaces()
        .inner_mut()
        .create()
        .call(create_args("x"))
        .await
        .unwrap();

    // Подписчик не читает: буфер потока переполняется, соединение — нет.
    let mut items = session
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::FLOOD))
        .await
        .map(|ok| ok.0)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        !client.peer().is_closed(),
        "ping must not starve behind a slow stream"
    );
    assert!(!accepted.peer.is_closed());
    let started = tokio::time::Instant::now();
    session.surfaces().get("x".into()).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));

    let mut delivered = 0;
    let last = loop {
        match items.next().await {
            Some(Ok(_)) => delivered += 1,
            Some(Err(e)) => break e,
            None => panic!("flood ended cleanly"),
        }
    };
    assert!(matches!(last, Generic::Aux(Aux::Quota(_))), "{last:?}");
    assert!(delivered <= client.peer().config().stream_buffer());
    assert!(delivered < FLOOD_ITEMS as usize);
    assert_eq!(items.next().await, None);

    tokio::time::timeout(
        Duration::from_secs(5),
        h.kernel.state.attach_dropped.notified(),
    )
    .await
    .expect("overflow must cancel the producer");
}

async fn max_frame_closes(via: Via) {
    let small = Config {
        max_frame: eva::bytesize::ByteSize::kib(64),
        ..config()
    };
    let mut h = Harness::with(via, small).await;

    // Клиенту лимит больше, чем серверу: кадр уходит и рвёт соединение у сервера.
    let (client, _) = RpcClient::connect_with(
        &h.target,
        hello(CLIENT_TOKEN),
        ClientOptions::default(),
        NoHandler,
    )
    .await
    .unwrap();
    let accepted = h.accepted().await;

    let mut args = create_args("x");
    args.name = "x".repeat(100 * 1024);
    let err = client
        .session()
        .surfaces()
        .inner_mut()
        .create()
        .call(args)
        .await
        .unwrap_err();
    // Причину закрытия клиент узнаёт уведомлением `close`.
    assert!(matches!(err, Generic::Aux(Aux::Invalid(_))), "{err:?}");

    let reason = tokio::time::timeout(Duration::from_secs(5), accepted.peer.closed())
        .await
        .unwrap();
    assert_eq!(reason.code, ErrorCode::InvalidParams);

    // Свой лимит клиент проверяет до отправки: соединение живо.
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;
    let too_big: Payload = codec::encode(&"x".repeat(100 * 1024)).unwrap();
    let err = client
        .peer()
        .call(method::HELLO, too_big)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidParams);
    assert!(!client.peer().is_closed());
}

async fn supersede_ends_streams(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, accepted) = h.connect(CLIENT_TOKEN).await;

    let mut items = client
        .session()
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::ENDLESS))
        .await
        .map(|ok| ok.0)
        .unwrap();
    assert_eq!(items.next().await, Some(Ok(item(0))));

    accepted
        .peer
        .close(RpcError::new(ErrorCode::Conflict, "superseded"));

    let end = tokio::time::timeout(Duration::from_secs(5), items.next())
        .await
        .unwrap();
    assert_eq!(
        end,
        Some(Err(Generic::Aux(Aux::Conflict("superseded".to_owned()))))
    );

    let reason = tokio::time::timeout(Duration::from_secs(5), client.closed())
        .await
        .unwrap();
    assert_eq!(
        reason.code,
        ErrorCode::Conflict,
        "the client learns why it was closed"
    );
}

async fn request_timeout_keeps_mutation(via: Via) {
    let mut h = Harness::start(via).await;
    *h.kernel.state.create_delay.lock().unwrap() = Duration::from_millis(600);
    let options = ClientOptions {
        config: Config {
            request_timeout: ms(150),
            ..config()
        },
        ..ClientOptions::default()
    };
    let (client, _) = RpcClient::connect_with(&h.target, hello(CLIENT_TOKEN), options, NoHandler)
        .await
        .unwrap();
    let _accepted = h.accepted().await;
    let mut session = client.session();

    let err = session
        .surfaces()
        .inner_mut()
        .create()
        .call(create_args("slow"))
        .await
        .unwrap_err();
    assert!(matches!(err, Generic::Aux(Aux::Unavailable(_))), "{err:?}");

    // `cancel` не обрывает мутацию: её эффект мог лечь, итог нужен окну.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let surface = session.surfaces().get("slow".into()).await.unwrap();
    assert_eq!(surface.surface.id.as_str(), "slow");
}

async fn heartbeat_keeps_long_call(via: Via) {
    // Ответ идёт 600 мс при `request_timeout` клиента 150 мс: живой запрос
    // держат heartbeat сервера раз в 50 мс.
    let mut h = Harness::with(
        via,
        Config {
            ping_interval: ms(50),
            ..config()
        },
    )
    .await;
    *h.kernel.state.create_delay.lock().unwrap() = Duration::from_millis(600);
    let options = ClientOptions {
        config: Config {
            request_timeout: ms(150),
            ..config()
        },
        ..ClientOptions::default()
    };
    let (client, _) = RpcClient::connect_with(&h.target, hello(CLIENT_TOKEN), options, NoHandler)
        .await
        .unwrap();
    let _accepted = h.accepted().await;

    let created = client
        .session()
        .surfaces()
        .inner_mut()
        .create()
        .call(create_args("slow"))
        .await
        .unwrap();
    assert_eq!(created.surface.id.as_str(), "slow");
}

async fn upload_of_small_pieces(via: Via) {
    let mut h = Harness::start(via).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;

    // Куски по 4 KiB склеиваются в чанки `blob_chunk`: буфер получателя
    // в 64 кадра не переполняется.
    let parts = (0..600_u32)
        .map(|i| vec![i as u8; 4096])
        .collect::<Vec<_>>();
    let whole = parts.concat();
    let ok = client
        .session()
        .blobs()
        .put(
            put::Meta {
                chat: chat_id(1),
                name: "small.bin".to_owned(),
                media_type: "application/octet-stream".to_owned(),
                kind: BlobKind::File,
                size: whole.len() as u64,
                hash: None,
            },
            body(parts),
        )
        .await
        .unwrap();
    assert_eq!(ok.size, whole.len() as u64);
    assert_eq!(*h.kernel.state.uploads.lock().unwrap(), vec![whole]);
}

async fn idle_stream_times_out(via: Via) {
    // Сервер шлёт heartbeat раз в 15 с, клиент ждёт кадр потока не дольше 200 мс.
    let mut h = Harness::start(via).await;
    let options = ClientOptions {
        config: Config {
            stream_idle_timeout: ms(200),
            ..config()
        },
        ..ClientOptions::default()
    };
    let (client, _) = RpcClient::connect_with(&h.target, hello(CLIENT_TOKEN), options, NoHandler)
        .await
        .unwrap();
    let _accepted = h.accepted().await;

    let mut items = client
        .session()
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::ENDLESS))
        .await
        .map(|ok| ok.0)
        .unwrap();
    assert_eq!(items.next().await, Some(Ok(item(0))));
    let end = tokio::time::timeout(Duration::from_secs(5), items.next())
        .await
        .unwrap();
    assert!(
        matches!(end, Some(Err(Generic::Aux(Aux::Unavailable(_))))),
        "{end:?}"
    );

    tokio::time::timeout(
        Duration::from_secs(5),
        h.kernel.state.attach_dropped.notified(),
    )
    .await
    .expect("idle timeout must cancel the producer");
}

async fn heartbeat_keeps_stream(via: Via) {
    let cfg = Config {
        ping_interval: ms(50),
        stream_idle_timeout: ms(200),
        ..config()
    };
    let mut h = Harness::with(via, cfg).await;
    let (client, _accepted) = h.connect(CLIENT_TOKEN).await;

    let mut items = client
        .session()
        .turns()
        .inner_mut()
        .attach()
        .call(attach_args(chat::ENDLESS))
        .await
        .map(|ok| ok.0)
        .unwrap();
    assert_eq!(items.next().await, Some(Ok(item(0))));

    let quiet = tokio::time::timeout(Duration::from_millis(700), items.next()).await;
    assert!(quiet.is_err(), "the stream must stay open: {quiet:?}");
}
