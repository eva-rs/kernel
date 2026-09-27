//! `modules.*` (`06 §2`, `§9`, `10 §2.6`, `11 §3`).

mod common;

use std::sync::atomic::Ordering;

use common::{CLIENT, OTHER, Outcome, SVC, TestSession, client, matrix_callers, outcome, service};
use eva_core::{CallStep as _, service::modules::Modules as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::modules::{list, token_issue, token_revoke},
    types::{
        ModuleId, PersonId,
        module::{State, TokenKind},
    },
    wire::{Capabilities, capabilities::ToolsCap},
};

use eva::utils::SecretString;
use eva_kernel::{
    config::modules::TokenEntry,
    db::{Db as _, Queries as _, modules::TokenRow},
};

fn issue(module: &str, kind: TokenKind) -> token_issue::Args {
    token_issue::Args {
        module: ModuleId::from(module),
        kind,
        issued_by: None,
    }
}

fn revoke(module: &str) -> token_revoke::Args {
    token_revoke::Args {
        module: ModuleId::from(module),
    }
}

fn hash(token: &str) -> Vec<u8> {
    blake3::hash(token.as_bytes()).as_bytes().to_vec()
}

/// Живой токен по хэшу: как его увидит handshake.
async fn live(h: &common::Harness, token: &str) -> Option<(ModuleId, TokenKind)> {
    let hash = hash(token);
    h.query(move |c| c.modules().token(&hash)).await
}

/// Живая строка `module_tokens` в обход `token_issue`.
async fn legacy_token(h: &common::Harness, module: &str, kind: TokenKind) {
    use eva::time::Clock as _;
    let row = TokenRow {
        hash: hash(&format!("{module}-legacy")),
        module: ModuleId::from(module),
        kind,
        issued_by: None,
        issued_via: Some(ModuleId::from(CLIENT)),
        issued_at: h.clock.get(),
    };
    h.kernel
        .shared()
        .db()
        .write(move |tx| tx.modules().insert_token(&row))
        .await
        .expect("legacy token");
}

async fn list_modules(session: &mut TestSession) -> Vec<eva_protocol::types::module::Module> {
    session
        .modules()
        .list()
        .call(list::Args {})
        .await
        .expect("modules.list")
        .modules
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
    assert_eq!(other.kind, None, "no token issued yet");
    assert!(modules[1].capabilities.is_empty());
}

#[tokio::test]
async fn list_kind_comes_from_live_token_and_config_wins() {
    let h = common::start_with(|cfg| {
        cfg.modules.tokens.insert(
            ModuleId::from(OTHER),
            TokenEntry {
                token: SecretString::Plaintext(String::from("configured")),
                kind: TokenKind::Client,
            },
        );
    })
    .await;
    h.register_module(SVC).await;
    h.register_module(OTHER).await;
    let mut client = h.client_session().await;

    client
        .modules()
        .token_issue()
        .call(issue(SVC, TokenKind::Service))
        .await
        .expect("issue");
    // Токен, выданный до того, как конфиг назвал модуль.
    legacy_token(&h, OTHER, TokenKind::Service).await;

    let modules = list_modules(&mut client).await;
    let kinds: Vec<_> = modules.iter().map(|m| (m.name.as_str(), m.kind)).collect();
    assert_eq!(
        kinds,
        [
            (OTHER, Some(TokenKind::Client)),
            (SVC, Some(TokenKind::Service))
        ],
        "config token is stronger than the issued one"
    );

    client
        .modules()
        .token_revoke()
        .call(revoke(SVC))
        .await
        .expect("revoke");
    let modules = list_modules(&mut client).await;
    assert_eq!(modules[1].kind, None, "revoked token is not live");
}

#[tokio::test]
async fn token_issue_stores_only_the_hash() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let token_issue::Ok { token } = client
        .modules()
        .token_issue()
        .call(issue("bridge", TokenKind::Service))
        .await
        .expect("issue");
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    type Row = (Vec<u8>, String, String, Option<String>, Option<String>);
    let rows: Vec<Row> = h
        .query(|c| {
            let mut stmt = c.prepare(
                "SELECT token_hash, module, kind, issued_by, issued_via FROM module_tokens",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .await;
    assert_eq!(
        rows,
        [(
            hash(&token),
            String::from("bridge"),
            String::from("service"),
            None,
            Some(String::from(CLIENT)),
        )],
        "only blake3(token) is stored"
    );
    assert_eq!(
        live(&h, &token).await,
        Some((ModuleId::from("bridge"), TokenKind::Service))
    );
}

#[tokio::test]
async fn token_issue_rotates_the_live_token() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let first = client
        .modules()
        .token_issue()
        .call(issue("bridge", TokenKind::Service))
        .await
        .expect("first")
        .token;
    let second = client
        .modules()
        .token_issue()
        .call(issue("bridge", TokenKind::Client))
        .await
        .expect("second")
        .token;

    assert_ne!(first, second);
    assert_eq!(
        live(&h, &first).await,
        None,
        "rotation revokes the old token"
    );
    assert_eq!(
        live(&h, &second).await,
        Some((ModuleId::from("bridge"), TokenKind::Client))
    );
}

#[tokio::test]
async fn token_issue_rejects_reserved_and_malformed_names() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    for reserved in ["kernel", "system", "local"] {
        let res = client
            .modules()
            .token_issue()
            .call(issue(reserved, TokenKind::Client))
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{reserved}: {res:?}");
    }

    for bad in ["", "Bridge", "has space", "a/b", "9lives", &"a".repeat(65)] {
        let res = client
            .modules()
            .token_issue()
            .call(issue(bad, TokenKind::Client))
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
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::system()),
            ..issue("bridge", TokenKind::Client)
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "{res:?}");

    let res = kernel
        .modules()
        .token_issue()
        .call(issue("bridge", TokenKind::Client))
        .await;
    assert_eq!(
        outcome(&res),
        Outcome::Invalid,
        "kernel names the actor: {res:?}"
    );

    let res = kernel
        .modules()
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::numbered(99)),
            ..issue("bridge", TokenKind::Client)
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::NotFound(what))) if what.contains("p99")),
        "{res:?}"
    );

    kernel
        .modules()
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::system()),
            ..issue("bridge", TokenKind::Client)
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

    let res = client.modules().token_revoke().call(revoke("bridge")).await;
    assert!(
        matches!(&res, Err(Generic::Spec(token_revoke::Err::NotFound(e))) if e.module.as_str() == "bridge"),
        "{res:?}"
    );

    let token = client
        .modules()
        .token_issue()
        .call(issue("bridge", TokenKind::Service))
        .await
        .expect("issue")
        .token;
    client
        .modules()
        .token_revoke()
        .call(revoke("bridge"))
        .await
        .expect("revoke");
    assert_eq!(live(&h, &token).await, None);

    let res = client.modules().token_revoke().call(revoke("bridge")).await;
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
        let mut session = h.session(caller.clone()).await;
        let mut modules = session.modules();

        let res = modules.list().call(list::Args {}).await;
        assert_eq!(outcome(&res), expect, "{who} list");

        let args = token_issue::Args {
            issued_by: caller.is_kernel().then(PersonId::system),
            ..issue("bridge", TokenKind::Service)
        };
        let res = modules.token_issue().call(args).await;
        assert_eq!(outcome(&res), expect, "{who} token_issue");

        let res = modules.token_revoke().call(revoke("bridge")).await;
        assert_eq!(outcome(&res), expect, "{who} token_revoke");
    }

    // Отказ Policy — до базы: чужой модуль не узнает, есть ли токен.
    let mut svc = h.session(service(SVC)).await;
    let res = svc.modules().token_revoke().call(revoke("nobody")).await;
    assert_eq!(outcome(&res), Outcome::Forbidden);
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let mut client = h.session(client(CLIENT)).await;
    h.db_fail.store(true, Ordering::Relaxed);

    let res = client.modules().list().call(list::Args {}).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
    let res = client
        .modules()
        .token_issue()
        .call(issue("bridge", TokenKind::Client))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}

/// Handshake не примет выданный токен модуля из `modules.tokens`: выдача —
/// отказ, а не мёртвый секрет (`06 §9`, конфиг сильнее).
#[tokio::test]
async fn token_issue_for_a_configured_module_is_a_conflict() {
    let h = common::start_with(common::with_module_tokens).await;
    let mut kernel = h.kernel_session().await;
    let mut owner = h.client_session().await;

    let res = owner
        .modules()
        .token_issue()
        .call(issue(SVC, TokenKind::Service))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Conflict(why))) if why.contains("modules.tokens")),
        "{res:?}"
    );
    let res = kernel
        .modules()
        .token_issue()
        .call(token_issue::Args {
            issued_by: Some(PersonId::system()),
            ..issue(OTHER, TokenKind::Client)
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Conflict, "kernel tool: {res:?}");

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
        .token_issue()
        .call(issue("tablet", TokenKind::Client))
        .await
        .expect("issue")
        .token;

    let mut tablet = h.session(client("tablet")).await;
    let res = tablet
        .modules()
        .token_issue()
        .call(issue("tablet", TokenKind::Client))
        .await;
    assert_eq!(outcome(&res), Outcome::Conflict, "{res:?}");
    assert_eq!(
        live(&h, &first).await,
        Some((ModuleId::from("tablet"), TokenKind::Client)),
        "the old token still authenticates"
    );

    tablet
        .modules()
        .token_issue()
        .call(issue("bridge", TokenKind::Service))
        .await
        .expect("other names are fine");
}
