mod common;

use eva::handling::CallStep as _;
use eva_kernel::{
    db::{Db as _, DbResult, Queries as _},
    subsystems::tools::{
        maintenance::{self, Group, GroupOp},
        registry, warm,
    },
};
use eva_protocol::{
    requests::Tools::{Trait as _, catalog},
    types::{ChatId, ModuleId, tool::ToolDecl},
};
use serde_json::json;
use std::collections::BTreeSet;

fn tool(path: &str) -> ToolDecl {
    ToolDecl {
        path: path.into(),
        description: "Find project records.".into(),
        input: json!({"type":"object","properties":{}}),
        master_only: false,
        trusted_only: false,
        needs: vec![],
        ends_turn: false,
        parallel_safe: true,
        waits_on_cancel: false,
        result: Default::default(),
        timeout: None,
        idempotent: true,
    }
}

#[tokio::test]
async fn catalog_registration_reconnect_and_overlay_share_one_registry() {
    let h = common::start().await;
    let shared = h.kernel.shared();
    let module = ModuleId::from("searcher");
    let declarations = vec![tool("project.lookup")];
    let first = registry::register(shared, &module, &[], &declarations)
        .await
        .unwrap();
    let second = registry::register(shared, &module, &[], &declarations)
        .await
        .unwrap();
    assert_eq!(first, second);
    let mut session = h.client_session().await;
    let entries = session
        .tools()
        .inner_mut()
        .catalog()
        .call(catalog::Args {
            path: Some("project".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tool_id, first[0]);
    let create = maintenance::group(
        shared,
        Group {
            op: GroupOp::Create,
            path: "lookup".into(),
            about: Some("Project search.".into()),
            to: None,
        },
        "master",
    )
    .await
    .unwrap();
    assert!(!create.is_error);
    let moved = maintenance::group(
        shared,
        Group {
            op: GroupOp::Move,
            path: "project.lookup".into(),
            to: Some("lookup".into()),
            about: None,
        },
        "master",
    )
    .await
    .unwrap();
    assert!(!moved.is_error);
    registry::register(shared, &module, &[], &declarations)
        .await
        .unwrap();
    assert_eq!(
        shared.tools().get().get(first[0]).unwrap().path(),
        "lookup.lookup"
    );
    assert_eq!(
        shared.tools().get().get(first[0]).unwrap().route,
        "project_lookup"
    );
    registry::offline(shared, &module).await.unwrap();
    assert!(!shared.tools().get().get(first[0]).unwrap().online);
    assert_eq!(
        registry::register(shared, &module, &[], &declarations)
            .await
            .unwrap(),
        first
    );
}

#[tokio::test]
async fn overlay_rejects_kernel_roots_and_collisions_without_partial_changes() {
    let h = common::start().await;
    let shared = h.kernel.shared();
    registry::register(
        shared,
        &ModuleId::from("other"),
        &[],
        &[
            tool("external.find"),
            tool("other.find"),
            tool("a_b.c"),
            tool("a.b_c"),
        ],
    )
    .await
    .unwrap();
    let denied = maintenance::group(
        shared,
        Group {
            op: GroupOp::Move,
            path: "external.find".into(),
            to: Some("tools".into()),
            about: None,
        },
        "master",
    )
    .await
    .unwrap();
    assert!(denied.is_error);
    assert!(shared.tools().get().resolve("external.find").len() == 1);
    let collision = maintenance::group(
        shared,
        Group {
            op: GroupOp::Move,
            path: "external.find".into(),
            to: Some("other".into()),
            about: None,
        },
        "master",
    )
    .await
    .unwrap();
    assert!(collision.is_error);
    let larger = shared.tools().get().resolve("a.b_c")[0].id;
    assert!(shared.tools().get().get(larger).unwrap().lost_collision);
    let before = shared
        .db()
        .reader()
        .await
        .unwrap()
        .run(|c| c.tools().placements())
        .await
        .unwrap();
    assert!(before.is_empty());
}

#[tokio::test]
async fn warm_budget_is_daily_and_loads_pin_tools() {
    let h = common::start_with(|cfg| {
        cfg.tools.budget_percent = 1;
        cfg.tools.warm.max_hide_per_night = 1;
    })
    .await;
    let shared = h.kernel.shared();
    let now = shared.now();
    let chat = shared.db().write(move |tx| DbResult::Ok(ChatId(tx.query_row("INSERT INTO chats(folder_id,kind,created_at,last_activity_at) SELECT id,'persistent',?1,?1 FROM folders WHERE surface_id='local' AND name='Unsorted' RETURNING id", [now.as_nanos().get() as i64], |r| r.get(0))?))).await.unwrap();
    let ids = registry::register(
        shared,
        &ModuleId::from("many"),
        &[],
        &(0..10)
            .map(|n| tool(&format!("many.tool{n}")))
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    let day = shared.day_zone().day(now);
    let day_insert = day.clone();
    let pinned = ids[0];
    shared
        .db()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO tool_stats(day,chat_id,tool_id,loads,last_at) VALUES(?1,?2,?3,2,?4)",
                (&day_insert, chat.0, pinned.0, now.as_nanos().get() as i64),
            )?;
            DbResult::Ok(())
        })
        .await
        .unwrap();
    let first = warm::recompute(shared, now).await.unwrap();
    assert!(first.chats > 0);
    let pins = shared
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.tools().pins(chat, now))
        .await
        .unwrap();
    assert!(pins.contains(&pinned));
    let warm = shared
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.tools().warm(chat, &day))
        .await
        .unwrap()
        .unwrap();
    assert!(warm.contains(&pinned));
    assert!(BTreeSet::from_iter(ids).difference(&warm).count() <= 1);
    assert_eq!(warm::recompute(shared, now).await.unwrap().chats, 0);
}

#[tokio::test]
async fn sleep_group_dry_run_validates_a_virtual_overlay_without_writes() {
    use eva_kernel::subsystems::sleep::groups::Phase;
    use eva_protocol::types::RunId;
    let h = common::start_with(|cfg| cfg.sleep.dry_run = true).await;
    let shared = h.kernel.shared();
    registry::register(
        shared,
        &ModuleId::from("source"),
        &[],
        &[tool("source.lookup")],
    )
    .await
    .unwrap();
    let mut phase = Phase::build(shared, RunId::from("groups-test"), shared.now())
        .await
        .unwrap();
    assert!(
        !phase
            .call(
                "sleep_groups_create",
                json!({"path":"lookup","about":"Search records.","why":"Shorter group"})
            )
            .await
            .is_error
    );
    assert!(
        !phase
            .call(
                "sleep_groups_move",
                json!({"tool":"source.lookup","to":"lookup","why":"Group by purpose"})
            )
            .await
            .is_error
    );
    assert!(
        phase
            .call(
                "sleep_groups_delete",
                json!({"path":"lookup","why":"Still has a tool"})
            )
            .await
            .is_error
    );
    assert!(
        phase
            .call(
                "sleep_groups_describe",
                json!({"path":"tools","about":"[kernel] bad","why":"No injection"})
            )
            .await
            .is_error
    );
    let summary = phase.finish().await.unwrap();
    assert_eq!((summary.applied, summary.discarded), (2, 2));
    assert!(summary.ops_json.contains("would_apply"));
    assert_eq!(shared.tools().get().resolve("source.lookup").len(), 1);
    assert!(shared.tools().get().resolve("lookup.lookup").is_empty());
    let placements = shared
        .db()
        .reader()
        .await
        .unwrap()
        .run(|conn| conn.tools().placements())
        .await
        .unwrap();
    assert!(placements.is_empty());
}

#[tokio::test]
async fn sleep_group_operations_require_reasons_obey_limits_and_persist_source() {
    use eva_kernel::subsystems::sleep::groups::Phase;
    use eva_protocol::types::RunId;
    let h = common::start_with(|cfg| cfg.sleep.groups.max_ops = 1).await;
    let shared = h.kernel.shared();
    let mut phase = Phase::build(shared, RunId::from("groups-test"), shared.now())
        .await
        .unwrap();
    assert!(
        phase
            .call(
                "sleep_groups_create",
                json!({"path":"lookup","about":"Search records.","why":""})
            )
            .await
            .is_error
    );
    assert!(
        !phase
            .call(
                "sleep_groups_create",
                json!({"path":"lookup","about":"Search records.","why":"Useful group"})
            )
            .await
            .is_error
    );
    assert!(
        phase
            .call(
                "sleep_groups_create",
                json!({"path":"another","about":"Other records.","why":"Too many"})
            )
            .await
            .is_error
    );
    let summary = phase.finish().await.unwrap();
    assert_eq!((summary.applied, summary.discarded), (1, 2));
    let source = shared
        .db()
        .reader()
        .await
        .unwrap()
        .run(|conn| {
            DbResult::Ok(conn.query_row(
                "SELECT source FROM tool_groups WHERE path='lookup'",
                [],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(source, "sleep");
}
