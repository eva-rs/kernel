//! `modules.*` (`06 §2`, `§9`, `10 §2.6`, `11 §3`): список, реестр сервисов
//! `add`/`remove` и токены клиентов — без транспорта; дозвон — `dial.rs`.

mod common;

use std::sync::atomic::Ordering;

use common::{CLIENT, OTHER, Outcome, SVC, TestSession, client, matrix_callers, outcome, service};
use eva_core::{CallStep as _, service::Modules::Trait as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::modules::{add, list, remove, token_issue, token_revoke},
    types::{
        ModuleId, PersonId,
        module::{Dial, ModuleKind, ServiceAddress, ServiceKey, State},
    },
    wire::{Capabilities, capabilities::ToolsCap},
};

use eva::utils::SecretString;
use eva_kernel::{
    config::modules::TokenEntry,
    db::{Db as _, Queries as _, modules::TokenRow},
};

const KEY: &str = "bridge-key-0123456789abcdef0123456789";

fn issue(module: &str) -> token_issue::Args {
    token_issue::Args {
        module: ModuleId::from(module),
        issued_by: None,
    }
}

fn revoke(module: &str) -> token_revoke::Args {
    token_revoke::Args {
        module: ModuleId::from(module),
    }
}

fn address(module: &str) -> ServiceAddress {
    ServiceAddress::Uds(format!("/run/eva-{module}/module.sock").into())
}

fn add(module: &str, key: Option<&str>) -> add::Args {
    add::Args {
        module: ModuleId::from(module),
        address: address(module),
        key: key.map(ServiceKey::from),
    }
}

fn remove(module: &str) -> remove::Args {
    remove::Args {
        module: ModuleId::from(module),
    }
}

fn hash(token: &str) -> Vec<u8> {
    blake3::hash(token.as_bytes()).as_bytes().to_vec()
}

/// Клиент живого токена по хэшу: как его увидит handshake.
async fn live(h: &common::Harness, token: &str) -> Option<ModuleId> {
    let hash = hash(token);
    h.query(move |c| c.modules().token(&hash)).await
}

/// Живая строка `module_tokens` в обход `token_issue`.
async fn db_token(h: &common::Harness, module: &str) {
    use eva::time::Clock as _;
    let row = TokenRow {
        hash: hash(&format!("{module}-db")),
        module: ModuleId::from(module),
        issued_by: None,
        issued_via: Some(ModuleId::from(CLIENT)),
        issued_at: h.clock.get(),
    };
    h.kernel
        .shared()
        .db()
        .write(move |tx| tx.modules().insert_token(&row))
        .await
        .expect("db token");
}

async fn list_modules(session: &mut TestSession) -> Vec<eva_protocol::types::module::Module> {
    session
        .modules()
        .list()
        .await
        .expect("modules.list")
        .modules
}

async fn services(h: &common::Harness) -> Vec<(String, String, String, String)> {
    h.query(|c| {
        let mut stmt = c.prepare(
            "SELECT module, address, key, added_via FROM module_services ORDER BY module",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

#[tokio::test]
async fn list_shows_connected_modules_with_capabilities() {
    let h = common::start().await;
    h.register_module(SVC).await;
    let caps = rmp_serde::to_vec_named(&Capabilities {
        tools: Some(ToolsCap::default()),
        ..Capabilities::default()
    })
    .expect("encode capabilities");
    let (name, now) = (ModuleId::from(OTHER), h.clock.clone());
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            use eva::time::Clock as _;
            tx.modules()
                .connected(&name, "2.0.0", "1.2", &caps, now.get())
        })
        .await
        .expect("connect other");

    let mut client = h.client_session().await;
    let modules = list_modules(&mut client).await;
    let names: Vec<&str> = modules.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, [OTHER, SVC], "sorted by name");

    let other = &modules[0];
    assert_eq!(other.state, State::Connected);
    assert_eq!(other.version, "2.0.0");
    assert_eq!(other.proto_version, "1.2");
    assert_eq!(other.capabilities, ["tools"]);
    assert_eq!(
        (other.kind, &other.dial),
        (None, &None),
        "neither a registry entry nor a token"
    );
    assert!(modules[1].capabilities.is_empty());
}

#[tokio::test]
async fn list_kind_comes_from_the_registry_and_live_tokens() {
    let h = common::start_with(|cfg| {
        cfg.modules.tokens.insert(
            ModuleId::from(OTHER),
            TokenEntry::new(SecretString::Plaintext(String::from("configured"))),
        );
        common::with_service(cfg, SVC);
    })
    .await;
    h.register_module(SVC).await;
    h.register_module(OTHER).await;
    h.register_module("tablet").await;
    h.register_module("bridge").await;
    let mut client = h.client_session().await;

    client
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await
        .expect("issue");
    client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(KEY)))
        .await
        .expect("add");
    // Токены, выданные до того, как конфиг назвал модуль: инертны.
    db_token(&h, OTHER).await;
    db_token(&h, SVC).await;

    let modules = list_modules(&mut client).await;
    let kinds: Vec<_> = modules
        .iter()
        .map(|m| (m.name.as_str(), m.kind, m.dial.as_ref().map(|d| d.config)))
        .collect();
    assert_eq!(
        kinds,
        [
            ("bridge", Some(ModuleKind::Service), Some(false)),
            (OTHER, Some(ModuleKind::Client), None),
            (SVC, Some(ModuleKind::Service), Some(true)),
            ("tablet", Some(ModuleKind::Client), None),
        ],
        "the config is stronger than the database"
    );

    client
        .modules()
        .inner_mut()
        .token_revoke()
        .call(revoke("tablet"))
        .await
        .expect("revoke");
    let modules = list_modules(&mut client).await;
    assert_eq!(modules[3].kind, None, "revoked token is not live");
}

/// Запись реестра без строки `modules`: `offline`, пустые версии, `since` —
/// `added_at` или старт ядра (`06 §9`).
#[tokio::test]
async fn a_never_dialed_registry_entry_is_listed_offline() {
    let mut h = common::start_with(|cfg| common::with_service(cfg, SVC)).await;
    let started = h.kernel.shared().started_at();
    h.clock.advance(std::time::Duration::from_secs(60));
    let mut client = h.client_session().await;
    client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(KEY)))
        .await
        .expect("add");

    let modules = list_modules(&mut client).await;
    let [bridge, svc] = modules.as_slice() else {
        panic!("two registry entries: {modules:?}");
    };
    for (module, config) in [(bridge, false), (svc, true)] {
        assert_eq!(
            (
                module.state,
                module.kind,
                module.version.as_str(),
                module.proto_version.as_str()
            ),
            (State::Offline, Some(ModuleKind::Service), "", ""),
            "{module:?}"
        );
        assert!(module.capabilities.is_empty());
        assert_eq!(
            module.dial.as_ref().map(|d| d.config),
            Some(config),
            "{module:?}"
        );
    }
    assert_eq!(
        bridge.dial,
        Some(Dial {
            address: address("bridge"),
            config: false,
            error: None,
        })
    );
    assert_eq!(svc.since, started, "a config entry dates from the start");
    assert!(bridge.since > started, "a runtime entry dates from add");
}

#[tokio::test]
async fn add_generates_a_key_once_and_stores_it_as_is() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let add::Ok { key } = client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", None))
        .await
        .expect("add without a key");
    let key = key.expect("the kernel generates the key").0;
    assert_eq!(key.len(), 64);
    assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(
        !format!("{:?}", ServiceKey(key.clone())).contains(&key),
        "Debug hides the key"
    );

    let add::Ok { key: shown } = client
        .modules()
        .inner_mut()
        .add()
        .call(add("mcp", Some(KEY)))
        .await
        .expect("add with a key");
    assert_eq!(shown, None, "a given key is not echoed");

    assert_eq!(
        services(&h).await,
        [
            (
                String::from("bridge"),
                String::from("uds:///run/eva-bridge/module.sock"),
                key,
                String::from(CLIENT),
            ),
            (
                String::from("mcp"),
                String::from("uds:///run/eva-mcp/module.sock"),
                String::from(KEY),
                String::from(CLIENT),
            ),
        ],
        "the kernel dials with the key: it is stored as is"
    );
}

#[tokio::test]
async fn add_rejects_bad_ids_and_weak_keys() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    for bad in [
        "kernel",
        "system",
        "local",
        "",
        "Bridge",
        "has space",
        "a/b",
        "9lives",
    ] {
        let res = client
            .modules()
            .inner_mut()
            .add()
            .call(add(bad, Some(KEY)))
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{bad:?}: {res:?}");
    }
    for weak in ["", "short", &"k".repeat(31), "\n"] {
        let res = client
            .modules()
            .inner_mut()
            .add()
            .call(add("bridge", Some(weak)))
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{weak:?}: {res:?}");
    }
    client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(&"k".repeat(32))))
        .await
        .expect("32 characters are enough");
}

/// Одно пространство имён (`06 §2`): сервис конфига или БД, токен конфига
/// или живой `module_tokens` — `Taken`; отозванный токен id не держит.
#[tokio::test]
async fn add_of_a_taken_id_is_taken() {
    let h = common::start_with(common::with_modules).await;
    let mut client = h.client_session().await;
    client
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await
        .expect("issue");
    client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(KEY)))
        .await
        .expect("add");

    for taken in [SVC, CLIENT, "tablet", "bridge"] {
        let res = client
            .modules()
            .inner_mut()
            .add()
            .call(add(taken, Some(KEY)))
            .await;
        assert!(
            matches!(&res, Err(Generic::Spec(add::Err::Taken(e))) if e.module.as_str() == taken),
            "{taken}: {res:?}"
        );
    }
    assert_eq!(services(&h).await.len(), 1, "nothing overwritten");

    client
        .modules()
        .inner_mut()
        .token_revoke()
        .call(revoke("tablet"))
        .await
        .expect("revoke");
    client
        .modules()
        .inner_mut()
        .add()
        .call(add("tablet", Some(KEY)))
        .await
        .expect("a revoked token does not hold the id");
}

#[tokio::test]
async fn remove_takes_only_runtime_entries() {
    let h = common::start_with(common::with_modules).await;
    let mut client = h.client_session().await;

    let res = client
        .modules()
        .inner_mut()
        .remove()
        .call(remove(SVC))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Conflict(why))) if why.contains("modules.services")),
        "config is stronger: {res:?}"
    );
    for unknown in ["bridge", CLIENT] {
        let res = client
            .modules()
            .inner_mut()
            .remove()
            .call(remove(unknown))
            .await;
        assert!(
            matches!(&res, Err(Generic::Spec(remove::Err::ServiceNotFound(e))) if e.module.as_str() == unknown),
            "{unknown}: {res:?}"
        );
    }

    client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(KEY)))
        .await
        .expect("add");
    h.register_module("bridge").await;
    client
        .modules()
        .inner_mut()
        .remove()
        .call(remove("bridge"))
        .await
        .expect("remove");
    assert!(services(&h).await.is_empty());
    let modules = list_modules(&mut client).await;
    let bridge = modules
        .iter()
        .find(|m| m.name.as_str() == "bridge")
        .expect("the modules row stays");
    assert_eq!((bridge.kind, &bridge.dial), (None, &None));

    let res = client
        .modules()
        .inner_mut()
        .remove()
        .call(remove("bridge"))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(remove::Err::ServiceNotFound(_)))),
        "second remove: {res:?}"
    );
    client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(KEY)))
        .await
        .expect("the id is free again");
}

#[tokio::test]
async fn token_issue_stores_only_the_hash() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let token_issue::Ok { token } = client
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await
        .expect("issue");
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    type Row = (Vec<u8>, String, Option<String>, Option<String>);
    let rows: Vec<Row> = h
        .query(|c| {
            let mut stmt =
                c.prepare("SELECT token_hash, module, issued_by, issued_via FROM module_tokens")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .await;
    assert_eq!(
        rows,
        [(
            hash(&token),
            String::from("tablet"),
            None,
            Some(String::from(CLIENT)),
        )],
        "only blake3(token) is stored"
    );
    assert_eq!(live(&h, &token).await, Some(ModuleId::from("tablet")));
}

#[tokio::test]
async fn token_issue_rotates_the_live_token() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let first = client
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await
        .expect("first")
        .token;
    let second = client
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await
        .expect("second")
        .token;

    assert_ne!(first, second);
    assert_eq!(
        live(&h, &first).await,
        None,
        "rotation revokes the old token"
    );
    assert_eq!(live(&h, &second).await, Some(ModuleId::from("tablet")));
}

#[tokio::test]
async fn token_issue_rejects_reserved_and_malformed_names() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    for reserved in ["kernel", "system", "local"] {
        let res = client
            .modules()
            .inner_mut()
            .token_issue()
            .call(issue(reserved))
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{reserved}: {res:?}");
    }

    for bad in ["", "Bridge", "has space", "a/b", "9lives", &"a".repeat(65)] {
        let res = client
            .modules()
            .inner_mut()
            .token_issue()
            .call(issue(bad))
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{bad:?}: {res:?}");
    }
}

#[tokio::test]
async fn issued_by_is_the_kernel_tool_path_only() {
    let h = common::start().await;
    let mut kernel = h.kernel_session().await;
    let mut client = h.client_session().await;

    let res = client
        .modules()
        .inner_mut()
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::system()),
            ..issue("tablet")
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "{res:?}");

    let res = kernel
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await;
    assert_eq!(
        outcome(&res),
        Outcome::Invalid,
        "kernel names the actor: {res:?}"
    );

    let res = kernel
        .modules()
        .inner_mut()
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::numbered(99)),
            ..issue("tablet")
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::NotFound(what))) if what.contains("p99")),
        "{res:?}"
    );

    kernel
        .modules()
        .inner_mut()
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::system()),
            ..issue("tablet")
        })
        .await
        .expect("kernel tool path");
    let row: (Option<String>, Option<String>) = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT issued_by, issued_via FROM module_tokens WHERE revoked_at IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await;
    assert_eq!(row, (Some(String::from("p0")), None));
}

#[tokio::test]
async fn token_revoke_needs_a_live_token() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let res = client
        .modules()
        .inner_mut()
        .token_revoke()
        .call(revoke("tablet"))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(token_revoke::Err::NotFound(e))) if e.module.as_str() == "tablet"),
        "{res:?}"
    );

    let token = client
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await
        .expect("issue")
        .token;
    client
        .modules()
        .inner_mut()
        .token_revoke()
        .call(revoke("tablet"))
        .await
        .expect("revoke");
    assert_eq!(live(&h, &token).await, None);

    let res = client
        .modules()
        .inner_mut()
        .token_revoke()
        .call(revoke("tablet"))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(token_revoke::Err::NotFound(_)))),
        "second revoke: {res:?}"
    );
}

#[tokio::test]
async fn policy_matrix() {
    let h = common::start().await;
    h.register_module(SVC).await;

    for (who, caller) in matrix_callers() {
        let expect = if caller.is_owner() {
            Outcome::Ok
        } else {
            Outcome::Forbidden
        };
        // Дозвон на произвольный адрес наводит только клиент хозяина.
        let client_only = if matches!(caller, eva_core::Caller::Client { .. }) {
            Outcome::Ok
        } else {
            Outcome::Forbidden
        };
        let mut session = h.session(caller.clone()).await;
        let mut modules_client = session.modules();
        let modules = modules_client.inner_mut();

        let res = modules.list().call(list::Args {}).await;
        assert_eq!(outcome(&res), expect, "{who} list");

        let res = modules.add().call(add("bridge", Some(KEY))).await;
        assert_eq!(outcome(&res), client_only, "{who} add");
        let res = modules.remove().call(remove("bridge")).await;
        assert_eq!(outcome(&res), client_only, "{who} remove");

        let args = token_issue::Args {
            issued_by: caller.is_kernel().then(PersonId::system),
            ..issue("tablet")
        };
        let res = modules.token_issue().call(args).await;
        assert_eq!(outcome(&res), expect, "{who} token_issue");

        let res = modules.token_revoke().call(revoke("tablet")).await;
        assert_eq!(outcome(&res), expect, "{who} token_revoke");
    }

    // Отказ Policy — до базы: чужой модуль не узнает, есть ли токен или сервис.
    let mut svc = h.session(service(SVC)).await;
    let res = svc
        .modules()
        .inner_mut()
        .token_revoke()
        .call(revoke("nobody"))
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden);
    let res = svc
        .modules()
        .inner_mut()
        .remove()
        .call(remove("nobody"))
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden);
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let mut client = h.session(client(CLIENT)).await;
    h.db_fail.store(true, Ordering::Relaxed);

    let res = client.modules().list().await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
    let res = client
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
    let res = client
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(KEY)))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
    let res = client
        .modules()
        .inner_mut()
        .remove()
        .call(remove("bridge"))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}

/// Handshake не примет выданный токен модуля из конфига или реестра: выдача —
/// отказ, а не мёртвый секрет (`06 §9`, конфиг сильнее, пространство одно).
#[tokio::test]
async fn token_issue_for_a_registered_module_is_a_conflict() {
    let h = common::start_with(common::with_modules).await;
    let mut kernel = h.kernel_session().await;
    let mut owner = h.client_session().await;

    let res = owner
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue(common::CLIENT2))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Conflict(why))) if why.contains("modules.tokens")),
        "{res:?}"
    );
    let res = owner
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue(SVC))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Conflict(why))) if why.contains("modules.services")),
        "{res:?}"
    );
    let res = kernel
        .modules()
        .inner_mut()
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::system()),
            ..issue(OTHER)
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Conflict, "kernel tool: {res:?}");

    owner
        .modules()
        .inner_mut()
        .add()
        .call(add("bridge", Some(KEY)))
        .await
        .expect("add");
    let res = owner
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("bridge"))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Conflict(why))) if why.contains("modules.add")),
        "{res:?}"
    );

    let rows: i64 = h
        .query(|c| Ok(c.query_row("SELECT count(*) FROM module_tokens", [], |r| r.get(0))?))
        .await;
    assert_eq!(rows, 0, "nothing minted");
}

/// Ротация закрыла бы сессию, которой уходит единственная копия нового токена.
#[tokio::test]
async fn a_module_cannot_rotate_the_token_of_its_own_session() {
    let h = common::start().await;
    let mut owner = h.client_session().await;
    let first = owner
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await
        .expect("issue")
        .token;

    let mut tablet = h.session(client("tablet")).await;
    let res = tablet
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("tablet"))
        .await;
    assert_eq!(outcome(&res), Outcome::Conflict, "{res:?}");
    assert_eq!(
        live(&h, &first).await,
        Some(ModuleId::from("tablet")),
        "the old token still authenticates"
    );

    tablet
        .modules()
        .inner_mut()
        .token_issue()
        .call(issue("laptop"))
        .await
        .expect("other names are fine");
}
