//! Счётчики здоровья M1 (`09 §5`, I11): сессии модулей, новые личности,
//! перенос чата со сменой trust, ошибки и нецененный расход инференса.

mod common;

use common::{CLIENT, Harness, SVC, with_modules};
use eva::handling::CallStep as _;
use eva_protocol::{
    requests::Inference::{self, Trait as _},
    types::{
        ModelId, SurfaceId,
        content::Block,
        folder,
        generate::{ProviderError, ProviderErrorKind, StopReason, Usage},
    },
    wire::{
        Capabilities,
        handshake::{Hello, Resume},
    },
};
use eva_providers::mock::Script;
use eva_rpc::NoHandler;
use eva_soul::db::{Db as _, DbResult};

async fn counters(h: &Harness) -> Vec<(String, String, i64)> {
    h.query(|c| {
        let mut stmt =
            c.prepare("SELECT metric, key, SUM(count) FROM health_stats GROUP BY metric, key ORDER BY metric, key")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

fn has(counters: &[(String, String, i64)], metric: &str, key: &str, count: i64) -> bool {
    counters
        .iter()
        .any(|(m, k, n)| m == metric && k == key && *n == count)
}

#[tokio::test]
async fn module_sessions_are_counted() {
    let h = common::start_with(with_modules).await;
    let listener = h.bind_service(SVC).await;
    let (_listening, _url) = h.listen().await;
    let hello = |resume| Hello {
        resume,
        ..common::dialed_hello(Capabilities::default())
    };

    let (first, welcome) = common::answer(&listener, hello(None), NoHandler)
        .await
        .expect("dialed");
    drop(first);
    let resume = Some(Resume {
        session_id: welcome.session_id,
    });
    let (second, _) = common::answer(&listener, hello(resume), NoHandler)
        .await
        .expect("redialed with resume");
    // Без слушателя следующий дозвон — провал.
    drop((second, listener));

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let counters = counters(&h).await;
        let failed = counters
            .iter()
            .any(|(m, k, n)| m == "modules.dial_failed" && k == SVC && *n >= 1);
        if has(&counters, "modules.disconnected", SVC, 2) && failed {
            assert!(has(&counters, "modules.connected", SVC, 2), "{counters:?}");
            assert!(has(&counters, "modules.reconnects", SVC, 1), "{counters:?}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{counters:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn persons_and_trust_moves_are_counted() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    for external_id in ["alice", "alice", "bob"] {
        client
            .persons()
            .resolve(SurfaceId::from("local"), external_id.to_owned(), None, None)
            .await
            .expect("resolve");
    }

    h.soul
        .shared()
        .db()
        .write(|tx| -> DbResult<()> {
            tx.execute(
                "INSERT INTO folders (surface_id, name, trusted) VALUES ('local', 'Open', 0)",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("folder");
    let chat = client
        .chats()
        .create(
            SurfaceId::from("local"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("chat")
        .chat;
    let open = folder::Selector::Name {
        surface: SurfaceId::from("local"),
        name: String::from("Open"),
    };
    for folder in [
        open,
        folder::Selector::Name {
            surface: SurfaceId::from("local"),
            name: String::from("Unsorted"),
        },
    ] {
        client
            .chats()
            .r#move(chat.id, folder, true)
            .await
            .expect("move");
    }

    let counters = counters(&h).await;
    assert!(
        has(&counters, "persons.created", "local", 2),
        "{counters:?}"
    );
    assert!(has(&counters, "chats.trust_moves", "", 2), "{counters:?}");
}

#[tokio::test]
async fn inference_errors_and_unpriced_spend_are_counted() {
    let h = common::start().await;
    h.register_module(CLIENT).await;
    let mut client = h.client_session().await;
    let ask = || Inference::complete::Args {
        model: Some(ModelId::from(common::MODEL)),
        system: String::from("Answer briefly."),
        input: vec![Block::text("hi")],
        schema: None,
        max_tokens: None,
        effort: None,
    };

    h.provider.push(
        Script::new()
            .text("ok")
            .usage(Usage {
                input: 3,
                output: 1,
                ..Usage::default()
            })
            .stop(StopReason::EndTurn),
    );
    client
        .inference()
        .inner_mut()
        .complete()
        .call(ask())
        .await
        .expect("complete");

    h.provider.push(Script::error(ProviderError {
        kind: ProviderErrorKind::Quota,
        http_status: Some(429),
        raw: String::from("slow down"),
        retry_after: None,
    }));
    client
        .inference()
        .inner_mut()
        .complete()
        .call(ask())
        .await
        .expect_err("provider error");

    let counters = counters(&h).await;
    assert!(
        has(&counters, "spend.unpriced", common::MODEL, 1),
        "{counters:?}"
    );
    assert!(
        has(&counters, "inference.errors", "quota", 1),
        "{counters:?}"
    );
}
