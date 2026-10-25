//! `agents.list` (`07 §2.3`): подагенты тимлида или все, непрочитанные
//! письма наверх, whitelist id, матрица `Policy`.

mod common;

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, service::Agents::Trait as _};
use eva_kernel::db::{Db as _, DbResult};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::agents::list,
    types::{ChatId, ToolId, agent::Status},
};

const SEC: i64 = 1_000_000_000;

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

/// Тимлиды 1 и 2 в `local`, подагенты 3, 4 у первого и 5 у второго.
async fn seed(h: &Harness) {
    let t = i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit");
    let chats: String = (1..=5)
        .map(|id| {
            let folder = if id <= 2 { "Unsorted" } else { "agents" };
            format!(
                "INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
                   SELECT {id}, id, 'persistent', {t}, {t} FROM folders
                    WHERE surface_id = 'local' AND name = '{folder}';"
            )
        })
        .collect();
    exec(
        h,
        format!(
            "{chats}
             INSERT INTO agents (id, parent_chat_id, name, task, tools, report_schema, master_turn, status, started_at, finished_at)
               VALUES (3, 1, 'scout', 'find the bug', '[1, 2]', '{{\"type\":\"object\"}}', 1, 'running', {t1}, NULL),
                      (4, 1, 'fixer', 'fix it', '[]', NULL, 1, 'done', {t2}, {t3}),
                      (5, 2, 'other', 'elsewhere', '[7]', NULL, 0, 'idle', {t1}, NULL);
             INSERT INTO agent_messages (agent_id, direction, body, created_at, read_at) VALUES
               (3, 'to_parent', 'found it', {t2}, NULL),
               (3, 'to_parent', 'and another', {t2}, NULL),
               (3, 'to_parent', 'already read', {t2}, {t3}),
               (3, 'to_agent', 'go on', {t2}, NULL);",
            t1 = t + SEC,
            t2 = t + 2 * SEC,
            t3 = t + 3 * SEC,
        ),
    )
    .await;
}

async fn list(h: &Harness, chat: Option<i64>) -> list::Ok {
    h.client_session()
        .await
        .agents()
        .list(chat.map(ChatId))
        .await
        .expect("agents.list")
}

#[tokio::test]
async fn list_by_lead_with_unread_letters() {
    let h = common::start().await;
    seed(&h).await;

    let list::Ok { agents } = list(&h, Some(1)).await;
    let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["scout", "fixer"], "spawn order");

    let scout = &agents[0];
    assert_eq!((scout.chat, scout.parent), (ChatId(3), ChatId(1)));
    assert_eq!(scout.task, "find the bug");
    assert_eq!(scout.tools, [ToolId(1), ToolId(2)]);
    assert_eq!(
        scout.report_schema,
        Some(serde_json::json!({"type": "object"}))
    );
    assert!(scout.master_turn);
    assert_eq!(scout.status, Status::Running);
    assert_eq!(scout.unread, 2, "only unread letters to the lead");
    assert!(scout.finished_at.is_none());

    let fixer = &agents[1];
    assert_eq!(fixer.status, Status::Done);
    assert!(fixer.tools.is_empty(), "an empty whitelist stays empty");
    assert_eq!(fixer.unread, 0);
    assert!(fixer.finished_at.is_some());
}

#[tokio::test]
async fn list_without_chat_returns_every_team() {
    let h = common::start().await;
    seed(&h).await;
    let all: Vec<_> = list(&h, None)
        .await
        .agents
        .into_iter()
        .map(|a| a.chat.get())
        .collect();
    assert_eq!(all, [3, 5, 4]);
    assert!(list(&h, Some(999)).await.agents.is_empty());
    assert!(
        list(&h, Some(3)).await.agents.is_empty(),
        "a subagent leads no one"
    );
}

#[tokio::test]
async fn corrupt_whitelist_is_a_db_error() {
    let h = common::start().await;
    seed(&h).await;
    exec(
        &h,
        String::from("UPDATE agents SET tools = 'oops' WHERE id = 4;"),
    )
    .await;
    let res = h
        .client_session()
        .await
        .agents()
        .list(Some(ChatId(1)))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}

#[tokio::test]
async fn agents_matrix() {
    use Outcome::{Forbidden as F, Ok as K};

    let h = common::start().await;
    seed(&h).await;
    h.service_with_surface(common::SVC).await;
    h.service_with_surface(common::OTHER).await;

    let mut failures = Vec::new();
    for ((who, caller), want) in common::matrix_callers().into_iter().zip([K, K, F, F]) {
        let got = outcome(
            &h.session(caller)
                .await
                .agents()
                .inner_mut()
                .list()
                .call(list::Args::default())
                .await,
        );
        if got != want {
            failures.push(format!("agents.list as {who}: want {want:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
