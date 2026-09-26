//! `notes.list` (`04 §1`, `§2`, `§5`): порядок секции, режим без пересчёта,
//! бюджет от окна модели чата, `tokens` калиброванным ratio, матрица `Policy`.

mod common;

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, service::notes::Notes as _};
use eva_kernel::db::{Db as _, DbResult};
use eva_protocol::{
    errors::Generic,
    requests::notes::list,
    types::{ChatId, note::Mode},
};

async fn exec(h: &Harness, sql: String) {
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<()> {
            tx.execute_batch(&sql)?;
            Ok(())
        })
        .await
        .expect("seed");
}

fn t0(h: &Harness) -> i64 {
    i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit")
}

/// Persistent-чат в `Unsorted` surface `surface`.
async fn chat(h: &Harness, surface: &str, model: Option<&str>) -> ChatId {
    let (surface, model, now) = (surface.to_owned(), model.map(str::to_owned), t0(h));
    let id = h
        .kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<i64> {
            tx.execute(
                "INSERT INTO chats (folder_id, kind, model, created_at, last_activity_at)
                 VALUES ((SELECT id FROM folders WHERE surface_id = ?1 AND name = 'Unsorted'),
                         'persistent', ?2, ?3, ?3)",
                (surface, model, now),
            )?;
            Ok(tx.last_insert_rowid())
        })
        .await
        .expect("chat");
    ChatId(id)
}

fn note(chat: ChatId, name: &str, body: &str, pinned: bool, at: i64, deleted: bool) -> String {
    format!(
        "INSERT INTO chat_notes (chat_id, name, description, body, prev_body, pinned, created_at, updated_at, deleted_at)
         VALUES ({chat}, '{name}', 'about {name}', '{body}', {prev}, {pinned}, {at}, {at}, {deleted});",
        chat = chat.get(),
        prev = if name == "a" { "'old a'" } else { "NULL" },
        pinned = i32::from(pinned),
        deleted = if deleted { at.to_string() } else { String::from("NULL") },
    )
}

async fn list(h: &Harness, chat: ChatId) -> list::Ok {
    h.client_session()
        .await
        .notes()
        .list()
        .call(list::Args { chat })
        .await
        .expect("notes.list")
}

fn tokens(text: &str, ratio: f64) -> u32 {
    (text.len() as f64 / ratio).ceil() as u32
}

#[tokio::test]
async fn list_keeps_section_order_and_skips_deleted() {
    let h = common::start().await;
    let c = chat(&h, "local", None).await;
    let t = t0(&h);
    exec(
        &h,
        [
            note(c, "a", "body a", false, t + 2, false),
            note(c, "b", "body b", true, t + 3, false),
            note(c, "c", "body c", false, t + 1, false),
            note(c, "d", "body d", false, t, true),
        ]
        .concat(),
    )
    .await;

    let list::Ok {
        notes,
        mode,
        tokens: got,
        budget,
    } = list(&h, c).await;
    let names: Vec<_> = notes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["b", "c", "a"], "pinned first, then created_at");
    assert_eq!(notes[2].prev_body.as_deref(), Some("old a"));
    assert!(notes[0].pinned);
    assert_eq!(mode, Mode::Inline, "no notes_state row yet");
    // Окно мок-модели 200k · 1% = 2000 < потолка 4000.
    assert_eq!(budget, 2000);
    let section =
        "[notes]\n## b | about b\nbody b\n\n## c | about c\nbody c\n\n## a | about a\nbody a";
    assert_eq!(got, tokens(section, 4.0));
}

#[tokio::test]
async fn list_reads_mode_without_recomputing_it() {
    let h = common::start().await;
    let c = chat(&h, "local", None).await;
    let t = t0(&h);
    exec(
        &h,
        format!(
            "{}INSERT INTO notes_state (chat_id, mode, since_at, squeeze_turns) VALUES ({}, 'index', {t}, 0);",
            note(c, "a", "body a", false, t, false),
            c.get()
        ),
    )
    .await;

    let snapshot = || {
        h.query(move |conn| {
            Ok(conn.query_row(
                "SELECT mode, since_at, squeeze_turns FROM notes_state WHERE chat_id = ?1",
                [c.get()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )?)
        })
    };
    let before = snapshot().await;
    assert_eq!(list(&h, c).await.mode, Mode::Index);
    assert_eq!(
        snapshot().await,
        before,
        "notes.list never writes notes_state"
    );
}

#[tokio::test]
async fn tokens_use_the_calibrated_ratio_of_the_chat_model() {
    let h = common::start().await;
    let c = chat(&h, "local", None).await;
    exec(
        &h,
        format!(
            "INSERT INTO token_ratios (model, bytes_per_token, samples, updated_at) VALUES ('{}', 2.0, 5, {});",
            common::MODEL,
            t0(&h)
        ),
    )
    .await;

    let got = list(&h, c).await;
    assert!(got.notes.is_empty());
    assert_eq!(got.tokens, tokens("[notes] empty", 2.0));
}

#[tokio::test]
async fn budget_follows_the_model_chain() {
    let h = common::start().await;
    // Модели нет в реестре: потолок, а не отказ.
    let unknown = chat(&h, "local", Some("offline-model")).await;
    assert_eq!(list(&h, unknown).await.budget, 4000);

    let folder_model = chat(&h, "local", None).await;
    exec(
        &h,
        String::from("UPDATE folders SET model = 'offline-model' WHERE surface_id = 'local' AND name = 'Unsorted';"),
    )
    .await;
    assert_eq!(
        list(&h, folder_model).await.budget,
        4000,
        "folder model wins over default"
    );
}

#[tokio::test]
async fn budget_and_switch_come_from_the_notes_section() {
    let h = common::start_with(|cfg| {
        cfg.notes.budget_percent = eva_kernel::config::notes::BudgetPercent::POS2;
        cfg.notes.budget_max_tokens = 100_000;
    })
    .await;
    let own = chat(&h, "local", Some(common::MODEL)).await;
    assert_eq!(list(&h, own).await.budget, 4000, "2% of the 200k window");

    let h = common::start_with(|cfg| cfg.notes.enabled = false).await;
    let own = chat(&h, "local", None).await;
    let res = h
        .client_session()
        .await
        .notes()
        .list()
        .call(list::Args { chat: own })
        .await;
    assert_eq!(outcome(&res), Outcome::Unavailable, "{res:?}");
}

#[tokio::test]
async fn unknown_chat_is_not_found() {
    let h = common::start().await;
    let res = h
        .client_session()
        .await
        .notes()
        .list()
        .call(list::Args { chat: ChatId(999) })
        .await;
    assert!(
        matches!(res, Err(Generic::Spec(list::Err::NotFound(ref e))) if e.chat == ChatId(999).into()),
        "{res:?}"
    );
}

#[tokio::test]
async fn notes_matrix() {
    use Outcome::{Forbidden as F, Ok as K};

    let h = common::start().await;
    h.service_with_surface(common::SVC).await;
    h.service_with_surface(common::OTHER).await;
    let own = chat(&h, common::SVC, None).await;

    let mut failures = Vec::new();
    for ((who, caller), want) in common::matrix_callers().into_iter().zip([K, K, F, F]) {
        let got = outcome(
            &h.session(caller)
                .await
                .notes()
                .list()
                .call(list::Args { chat: own })
                .await,
        );
        if got != want {
            failures.push(format!("notes.list as {who}: want {want:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
