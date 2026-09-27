//! Пределы, которые держат ядро от чужого пира: `max_hello` и места до
//! `welcome`, срок подключения клиента, байты загрузок.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use futures::StreamExt as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpSocket, TcpStream, UnixStream},
};
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

use eva::{bytesize::ByteSize, url::Url};
use eva_protocol::{
    requests::blobs::put,
    types::{Bytes, blob::Kind as BlobKind},
    wire::{
        ErrorCode, Frame, RpcError,
        frame::{StreamEnd, StreamItem},
        method,
    },
};
use eva_rpc::{
    ClientOptions, ConnectError, NoHandler, RpcClient, codec,
    config::{Config, StreamBuffer},
};

use crate::{
    harness::{Harness, Raw, Via, config, hello, ms},
    kernel::{CLIENT_TOKEN, chat_id},
};

fn small_hello() -> Config {
    Config {
        max_hello: ByteSize::kib(64),
        ..config()
    }
}

/// Кадр больше `max_hello` ядро не дочитывает: причина `invalid_params`
/// приходит уведомлением `close` сразу, а не по `handshake_timeout`.
#[tokio::test(flavor = "multi_thread")]
async fn hello_over_max_hello_is_refused() {
    let h = Harness::with(Via::Uds, small_hello()).await;
    let stream = UnixStream::connect(h.socket.as_ref().unwrap())
        .await
        .unwrap();
    let (rd, mut wr) = stream.into_split();
    // Кадр в мегабайт укладывается в `max_frame`, но не в `max_hello`.
    let writer = tokio::spawn(async move {
        _ = wr.write_all(&(1_u32 << 20).to_be_bytes()).await;
        _ = wr.write_all(&vec![0_u8; 256 << 10]).await;
        wr
    });

    let codec = LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_type::<u32>()
        .new_codec();
    let mut frames = FramedRead::new(rd, codec);
    // Раньше `handshake_timeout` (2 с): закрывает бюджет, а не таймер.
    let raw = tokio::time::timeout(Duration::from_secs(1), frames.next())
        .await
        .expect("the kernel waited for the whole oversized hello")
        .expect("closed without a reason")
        .unwrap();
    let Frame::Notify(notify) = codec::decode_frame(&raw).unwrap() else {
        panic!("expected notify close");
    };
    assert_eq!(notify.method, method::CLOSE);
    let reason: RpcError = codec::decode(&notify.params).unwrap();
    assert_eq!(reason.code, ErrorCode::InvalidParams);
    assert!(reason.message.contains("max_hello"), "{reason}");
    drop(writer.await);
}

/// Модуль с `max_hello` больше ядерного получает отказ по существу,
/// который SDK не повторяет, а не обрыв транспорта.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_hello_is_rejected_not_dropped() {
    for via in [Via::Uds, Via::Ws] {
        let h = Harness::with(via, small_hello()).await;
        let mut big = hello(CLIENT_TOKEN);
        big.version = "x".repeat(1 << 20);

        let started = tokio::time::Instant::now();
        let result =
            RpcClient::connect_with(&h.target, big, ClientOptions::default(), NoHandler).await;
        match result {
            Err(ConnectError::Rejected(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidParams, "{via:?}: {e}");
            }
            other => panic!("{via:?}: expected a rejection, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(1), "{via:?}");
    }
}

/// Свой `max_hello` клиент проверяет до сети: адреса без ядра он даже не касается.
#[tokio::test(flavor = "multi_thread")]
async fn hello_over_own_max_hello_is_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let nowhere = Url::parse(&format!("uds://{}", dir.path().join("none.sock").display())).unwrap();
    let mut big = hello(CLIENT_TOKEN);
    big.version = "x".repeat(100 << 10);
    let options = ClientOptions {
        config: small_hello(),
        ..ClientOptions::default()
    };

    let result = RpcClient::connect_with(&nowhere, big, options, NoHandler).await;
    assert!(
        matches!(&result, Err(ConnectError::Config(message)) if message.contains("max_hello")),
        "{result:?}"
    );
}

/// Сокет с адреса `ip` к слушателю `to`.
async fn tcp_from(ip: Ipv4Addr, to: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(IpAddr::V4(ip), 0)).unwrap();
    socket.connect(to).await.unwrap()
}

/// Закрыт ли сокет сервером за `within`.
async fn dropped(stream: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0_u8; 16];
    matches!(
        tokio::time::timeout(within, stream.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

/// Молчащие сокеты занимают места до `welcome`, но новичок вытесняет
/// старейший из них: и из той же сети, и при полном лимите сервера.
#[tokio::test(flavor = "multi_thread")]
async fn silent_sockets_only_delay_modules() {
    let cfg = Config {
        handshake_timeout: ms(10_000),
        ..config()
    };
    let h = Harness::with(Via::Ws, cfg).await;
    let addr: SocketAddr = format!(
        "{}:{}",
        h.target.host_str().unwrap(),
        h.target.port().unwrap()
    )
    .parse()
    .unwrap();
    let home = Ipv4Addr::LOCALHOST;

    // Доля одной сети — 8 мест; девятый вытесняет старейшего из них.
    let mut own = Vec::new();
    for _ in 0..8 {
        own.push(tcp_from(home, addr).await);
    }
    let mut ninth = tcp_from(home, addr).await;
    assert!(dropped(&mut own[0], Duration::from_secs(2)).await);
    assert!(!dropped(&mut ninth, Duration::from_millis(300)).await);
    drop((own, ninth));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Все 64 места у восьми чужих сетей: модуль всё равно входит.
    let mut flood = Vec::new();
    for host in 2..10 {
        for _ in 0..8 {
            flood.push(tcp_from(Ipv4Addr::new(127, 0, 0, host), addr).await);
        }
    }
    let result =
        RpcClient::connect_with(&h.target, hello(CLIENT_TOKEN), h.options(), NoHandler).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(dropped(&mut flood[0], Duration::from_secs(2)).await);
}

/// Путь, пропавший после TCP-handshake, не держит подключение дольше
/// `handshake_timeout`.
#[tokio::test(flavor = "multi_thread")]
async fn transport_connect_has_a_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _mute = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let target = Url::parse(&format!("ws://{addr}/v1/modules")).unwrap();
    let options = ClientOptions {
        config: Config {
            handshake_timeout: ms(200),
            ..config()
        },
        ..ClientOptions::default()
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        RpcClient::connect_with(&target, hello(CLIENT_TOKEN), options, NoHandler),
    )
    .await
    .expect("connect outlived handshake_timeout");
    assert!(
        matches!(result, Err(ConnectError::Transport(_))),
        "{result:?}"
    );
}

fn put(size: usize) -> put::Meta {
    put::Meta {
        chat: chat_id(1),
        name: "file.bin".to_owned(),
        media_type: "application/octet-stream".to_owned(),
        kind: BlobKind::File,
        size: size as u64,
        hash: None,
    }
}

async fn chunk(raw: &mut Raw, id: u64, seq: u64, bytes: usize) {
    raw.send(&Frame::StreamItem(StreamItem {
        id,
        seq,
        item: codec::encode(&Bytes(vec![1; bytes])).unwrap(),
    }))
    .await;
}

async fn end(raw: &mut Raw, id: u64) {
    raw.send(&Frame::StreamEnd(StreamEnd { id, error: None }))
        .await;
}

/// Чанк крупнее `blob_chunk` рвёт загрузку с `invalid_params`: буфер
/// загрузки считает чанки, а не байты.
#[tokio::test(flavor = "multi_thread")]
async fn upload_chunk_over_blob_chunk_is_refused() {
    let cfg = Config {
        blob_chunk: ByteSize::kib(64),
        ..config()
    };
    let h = Harness::with(Via::Uds, cfg).await;
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();

    let size = 128 << 10;
    raw.request(1, "blobs.put", codec::encode(&put(size)).unwrap())
        .await;
    chunk(&mut raw, 1, 0, size).await;
    end(&mut raw, 1).await;

    let error = raw.response(1).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidParams, "{error}");
    assert!(error.message.contains("blob_chunk"), "{error}");
    assert!(h.kernel.state.uploads.lock().unwrap().is_empty());
}

/// Загрузки одной сессии вместе держат не больше `stream_buffer × blob_chunk`
/// непрочитанных байт; лишняя получает `quota`, остальные доходят.
#[tokio::test(flavor = "multi_thread")]
async fn upload_buffers_are_capped_per_session() {
    let cfg = Config {
        blob_chunk: ByteSize::kib(64),
        stream_buffer: StreamBuffer::POS16,
        ..config()
    };
    let h = Harness::with(Via::Uds, cfg).await;
    *h.kernel.state.put_delay.lock().unwrap() = Duration::from_millis(300);
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();

    // Каждая загрузка укладывается в свой буфер (12 из 16 чанков), но две
    // вместе — нет.
    let chunks = 12;
    let size = chunks * (64 << 10);
    for id in [1, 2] {
        raw.request(id, "blobs.put", codec::encode(&put(size)).unwrap())
            .await;
        for seq in 0..chunks {
            chunk(&mut raw, id, seq as u64, 64 << 10).await;
        }
        end(&mut raw, id).await;
    }

    let first: put::Ok = codec::decode(&raw.response(1).await.unwrap()).unwrap();
    assert_eq!(first.size, size as u64);
    let error = raw.response(2).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Quota, "{error}");
}
