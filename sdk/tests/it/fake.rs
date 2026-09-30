//! Поддельное ядро: handshake руками, `surfaces.*` в памяти, вызовы ядро →
//! модуль через `ModuleClient`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use eva::{time::Timestamp, utils::SecretString};
use eva_protocol::{
    errors::{self, Generic},
    requests::surfaces::{create, get, update},
    types::{
        ChatId, ModuleId, SessionId, SurfaceId, ToolId, TurnId,
        module::ModuleKind,
        surface::Surface,
        turn::{Actor, TurnCtx},
    },
    wire::{
        Frame, Hello, RpcError, Welcome,
        frame::{Payload, Response},
        handshake::ProtoVersion,
        method,
    },
};
use eva_rpc::{
    Call, Config, Conn, IdSeq, Kind, ModuleClient, Peer, Service, codec, transport::Outbound,
};

/// Id модуля, который поддельное ядро нашло по токену.
pub const MODULE: &str = "x-bridge";

/// Токен в форме конфига.
pub fn token(value: &str) -> SecretString {
    SecretString::Plaintext(value.to_owned())
}

pub fn welcome(session: &str, tool_ids: &[(&str, i64)]) -> Welcome {
    Welcome {
        session_id: SessionId::from(session),
        module: ModuleId::from(MODULE),
        kernel_version: "0.0.0-test".to_owned(),
        proto_version: ProtoVersion::CURRENT,
        kind: ModuleKind::Service,
        services: Vec::new(),
        tool_ids: tool_ids
            .iter()
            .map(|(path, id)| ((*path).to_owned(), ToolId(*id)))
            .collect(),
        models_accepted: Vec::new(),
        warnings: Vec::new(),
        resumed: false,
    }
}

/// `welcome` сессии, продолжившей окно идемпотентности прошлой.
pub fn resumed(session: &str) -> Welcome {
    Welcome {
        resumed: true,
        ..welcome(session, &[])
    }
}

pub fn turn() -> TurnCtx {
    TurnCtx {
        turn_id: TurnId::from("t-1"),
        chat: ChatId(1),
        actor: Actor::unknown(),
        master_turn: false,
        trusted_chat: false,
        public: true,
        surface: SurfaceId::from("x"),
        anchor: None,
        run_id: None,
    }
}

/// Принятая поддельным ядром сессия.
pub struct Session {
    pub hello: Hello,
    /// `id` запроса `hello`: по нему видно продолжение счётчика после resume.
    pub hello_id: u64,
    pub peer: Peer,
}

impl Session {
    pub fn module(&self) -> ModuleClient {
        ModuleClient::new(self.peer.clone())
    }
}

/// Отвечает на `hello` и поднимает пир ядра над `service`.
pub async fn accept<S: Service>(mut conn: Conn, welcome: Welcome, service: S) -> Session {
    let raw = conn
        .recv()
        .await
        .expect("hello frame")
        .expect("readable frame");
    let Frame::Request(request) = codec::decode_frame(&raw).expect("frame") else {
        panic!("the first frame must be a request");
    };
    assert_eq!(request.method, method::HELLO);
    let hello: Hello = codec::decode(&request.params).expect("hello");

    let reply = Frame::Response(Response::ok(
        request.id,
        codec::encode(&welcome).expect("welcome"),
    ));
    conn.send(Outbound::Frame(codec::encode_frame(&reply).expect("frame")))
        .await
        .expect("send welcome");

    let peer = Peer::spawn(conn, Config::default(), IdSeq::server(), service);
    Session {
        hello,
        hello_id: request.id,
        peer,
    }
}

/// `surfaces.get/create/update` в памяти и журнал вызовов.
#[derive(Clone, Default)]
pub struct Surfaces {
    pub rows: Arc<Mutex<BTreeMap<SurfaceId, Surface>>>,
    pub calls: Arc<Mutex<Vec<String>>>,
    /// `(request.id, method)` в порядке прихода.
    pub requests: Arc<Mutex<Vec<(u64, String)>>>,
    pub creates: Arc<Mutex<Vec<create::Args>>>,
    pub updates: Arc<Mutex<Vec<update::Args>>>,
}

impl Surfaces {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn row(id: SurfaceId, name: String, rules: Option<String>) -> Surface {
        Surface {
            id,
            name,
            module: None,
            rules,
            trusted: None,
            model: None,
            effort: None,
            thinking: None,
            compact_at: None,
            builtin: None,
            created_at: Timestamp::from_secs(1_750_000_000),
        }
    }
}

fn spec<E: serde::Serialize + errors::HasCode + std::fmt::Display>(error: E) -> RpcError {
    codec::typed(&Generic::<E>::Spec(error))
}

impl Service for Surfaces {
    fn kind(&self, method: &str) -> Option<Kind> {
        matches!(
            method,
            "surfaces.get" | "surfaces.create" | "surfaces.update"
        )
        .then_some(Kind::Unary)
    }

    async fn unary(&self, call: Call) -> Result<Payload, RpcError> {
        self.calls.lock().unwrap().push(call.method.clone());
        self.requests
            .lock()
            .unwrap()
            .push((call.id, call.method.clone()));
        let mut rows = self.rows.lock().unwrap();
        match call.method.as_str() {
            "surfaces.get" => {
                let args: get::Args = codec::decode(&call.params).map_err(codec::bad_params)?;
                match rows.get(&args.surface) {
                    Some(surface) => Ok(codec::encode(&get::Ok {
                        surface: surface.clone(),
                    })
                    .unwrap()),
                    None => Err(spec(get::Err::from(errors::surfaces::NotFound {
                        surface: args.surface,
                    }))),
                }
            }
            "surfaces.create" => {
                let args: create::Args = codec::decode(&call.params).map_err(codec::bad_params)?;
                self.creates.lock().unwrap().push(args.clone());
                let surface = Self::row(args.id.clone(), args.name, args.rules);
                rows.insert(args.id, surface.clone());
                Ok(codec::encode(&create::Ok { surface }).unwrap())
            }
            "surfaces.update" => {
                let args: update::Args = codec::decode(&call.params).map_err(codec::bad_params)?;
                self.updates.lock().unwrap().push(args.clone());
                let surface = rows
                    .get(&args.surface)
                    .cloned()
                    .expect("updated surface exists");
                Ok(codec::encode(&update::Ok { surface }).unwrap())
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }
}
