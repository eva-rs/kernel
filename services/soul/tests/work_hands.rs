mod common;
mod turns_support;
use eva_protocol::types::{ChatId, PersonId, SurfaceId, hands::GrantKind, turn::TurnOutcome};
use eva_providers::mock::Script;
use eva_soul::{
    db::{Db as _, DbResult, Queries as _, hands_grants::NewGrant},
    subsystems::turns::log::{Payload, ResultContent},
};
use serde_json::json;
use turns_support::*;

async fn working_chat(h: &common::Harness) -> ChatId {
    let chat = chat(h).await;
    let cwd = h.dir.path().join("work").to_string_lossy().into_owned();
    h.soul
        .shared()
        .db()
        .write(move |tx| {
            tx.execute("UPDATE chats SET workdir = ?2 WHERE id = ?1", (chat.0, cwd))?;
            DbResult::Ok(())
        })
        .await
        .unwrap();
    chat
}

#[tokio::test]
async fn find_reports_omitted_matches_by_first_level_directory() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    grant(&h, chat).await;
    let root = h.dir.path().join("work");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join("a/one.txt"), "hit one\nhit two\n").unwrap();
    std::fs::write(root.join("b/two.txt"), "hit three\n").unwrap();
    h.provider.push(Script::call(
        "find",
        "fs_find",
        json!({
            "pattern": "hit", "path": ".", "limit": 1
        }),
    ));
    h.provider.push(Script::reply("done"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "find matches").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let result = events(&h, chat)
        .await
        .into_iter()
        .find_map(|event| match event.payload {
            Payload::ToolResult(result) if !result.is_error => Some(result),
            _ => None,
        })
        .expect("find tool result");
    let shown = format!("{:?}", result.content);
    assert!(shown.contains("[2 more:"), "{shown}");
    assert!(
        shown.contains("/a/ 1") && shown.contains("/b/ 1"),
        "{shown}"
    );
}
async fn grant(h: &common::Harness, chat: ChatId) {
    let row = NewGrant {
        endpoint: "local".into(),
        surface: SurfaceId::from("local"),
        chat: Some(chat),
        kind: GrantKind::Write,
        prefix: h.dir.path().join("work").to_string_lossy().into_owned(),
        issued_by: PersonId::system(),
        created_at: h.soul.shared().now(),
    };
    h.soul
        .shared()
        .db()
        .write(move |tx| tx.hands_grants().insert(&row))
        .await
        .unwrap();
}

async fn shell_grant(h: &common::Harness, chat: ChatId, command: &str) {
    let row = NewGrant {
        endpoint: "local".into(),
        surface: SurfaceId::from("local"),
        chat: Some(chat),
        kind: GrantKind::Shell,
        prefix: command.into(),
        issued_by: PersonId::system(),
        created_at: h.soul.shared().now(),
    };
    h.soul
        .shared()
        .db()
        .write(move |tx| tx.hands_grants().insert(&row))
        .await
        .unwrap();
}

async fn shell_result(h: &common::Harness, chat: ChatId) -> String {
    let result = events(h, chat)
        .await
        .into_iter()
        .find_map(|event| match event.payload {
            Payload::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("shell result");
    assert!(!result.is_error, "{result:?}");
    result
        .content
        .into_iter()
        .filter_map(|part| match part {
            ResultContent::Text { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn shell_started(h: &common::Harness) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !h.dir.path().join("work/started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shell started");
}

#[tokio::test]
async fn shell_preserves_exit_code_and_separate_output_streams() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    let command = "printf 'stdout line\\n'; printf 'stderr line\\n' >&2; exit 7";
    shell_grant(&h, chat, command).await;
    h.provider.push(Script::call(
        "exec",
        "shell_exec",
        json!({"command": command}),
    ));
    h.provider.push(Script::reply("done"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "run command").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    assert_eq!(
        shell_result(&h, chat).await,
        "exit 7 signal none\n[tail: lines 1-1 of 1]\nstdout line\n\n[stderr]\n[tail: lines 1-1 of 1]\nstderr line\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_reports_signal_without_inventing_exit_code() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    let command = "kill -TERM $$";
    shell_grant(&h, chat, command).await;
    h.provider.push(Script::call(
        "exec",
        "shell_exec",
        json!({"command": command}),
    ));
    h.provider.push(Script::reply("done"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "run command").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let result = shell_result(&h, chat).await;
    assert_eq!(result.lines().next(), Some("exit none signal SIGTERM"));
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_preserves_trapped_exit_and_partial_output() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    let command = "trap 'exit 0' TERM; printf 'before timeout\\n'; printf 'stderr before timeout\\n' >&2; while :; do sleep 1; done";
    shell_grant(&h, chat, command).await;
    h.provider.push(Script::call(
        "exec",
        "shell_exec",
        json!({"command": command, "timeout": "500ms"}),
    ));
    h.provider.push(Script::reply("done"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "run command").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let result = shell_result(&h, chat).await;
    assert_eq!(
        result.lines().next(),
        Some("exit 0 signal none timed out after 500ms")
    );
    let (stdout, stderr) = result.split_once("[stderr]\n").expect("stderr section");
    assert!(stdout.contains("before timeout\n"), "{result}");
    assert!(stderr.contains("stderr before timeout\n"), "{result}");
}

#[tokio::test]
async fn shell_cancellation_waits_for_real_exit_and_output() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    let command =
        ": > started; while [ ! -e release ]; do sleep 0.01; done; printf 'finished\\n'; exit 7";
    shell_grant(&h, chat, command).await;
    h.provider.push(Script::call(
        "exec",
        "shell_exec",
        json!({"command": command, "timeout": "5s"}),
    ));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "run command").await;
    shell_started(&h).await;
    let live = h
        .soul
        .shared()
        .turns()
        .slot(chat)
        .live()
        .expect("live turn");
    let mut session = h.client_session().await;
    session.turns().cancel(chat).await.unwrap();
    assert!(live.cancel.is_cancelled());
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            live.closed.cancelled()
        )
        .await
        .is_err(),
        "shell must finish before the cancelled turn closes"
    );
    std::fs::write(h.dir.path().join("work/release"), "").unwrap();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Cancelled);
    let result = shell_result(&h, chat).await;
    assert_eq!(result.lines().next(), Some("exit 7 signal none cancelled"));
    assert!(result.contains("finished\n"), "{result}");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_and_cancellation_are_independent() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    let command = "trap 'exit 0' TERM; : > started; while :; do sleep 1; done";
    shell_grant(&h, chat, command).await;
    h.provider.push(Script::call(
        "exec",
        "shell_exec",
        json!({"command": command, "timeout": "1s"}),
    ));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "run command").await;
    shell_started(&h).await;
    let mut session = h.client_session().await;
    session.turns().cancel(chat).await.unwrap();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Cancelled);
    let result = shell_result(&h, chat).await;
    assert_eq!(
        result.lines().next(),
        Some("exit 0 signal none timed out after 1s cancelled")
    );
}

#[tokio::test]
async fn read_before_write_guard_then_read_and_edit() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    grant(&h, chat).await;
    let file = h.dir.path().join("work/a.txt");
    std::fs::write(&file, "first\nlast\n").unwrap();
    h.provider.push(Script::call(
        "blind",
        "fs_write",
        json!({"path":"a.txt","content":"clobber"}),
    ));
    h.provider.push(Script::call(
        "read",
        "fs_read",
        json!({"path":"a.txt","offset":1,"limit":1}),
    ));
    h.provider.push(Script::call(
        "edit",
        "fs_edit",
        json!({"path":"a.txt","old":"first","new":"updated"}),
    ));
    h.provider.push(Script::reply("done"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "update file").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    assert_eq!(std::fs::read_to_string(file).unwrap(), "updated\nlast\n");
    let events = events(&h, chat).await;
    assert!(events.iter().any(|e| matches!(&e.payload, Payload::ToolResult(result) if result.is_error && format!("{:?}",result.content).contains("read it first"))));
    assert!(events.iter().any(|e| matches!(&e.payload, Payload::ToolResult(result) if format!("{:?}",result.content).contains("read on with offset 2"))));
}

#[tokio::test]
async fn invalid_patch_writes_nothing_and_valid_patch_commits_all_files() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    grant(&h, chat).await;
    let file = h.dir.path().join("work/a.txt");
    std::fs::write(&file, "old\n").unwrap();
    h.provider.push(Script::call("bad", "fs_patch", json!({"patch":"*** Begin Patch\n*** Add File: new.txt\n+new\n*** Update File: a.txt\n@@\n-missing\n+oops\n*** End Patch"})));
    h.provider.push(Script::reply("stopped"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "try patch").await;
    done(&mut rx).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    assert!(!h.dir.path().join("work/new.txt").exists());
    h.provider.push(Script::call("good", "fs_patch", json!({"patch":"*** Begin Patch\n*** Add File: new.txt\n+new\n*** Update File: a.txt\n@@\n-old\n+updated\n*** End Patch"})));
    h.provider.push(Script::reply("patched"));
    append(&h, chat, "valid patch").await;
    done(&mut rx).await;
    assert_eq!(std::fs::read_to_string(file).unwrap(), "updated\n");
    assert_eq!(
        std::fs::read_to_string(h.dir.path().join("work/new.txt")).unwrap(),
        "new\n"
    );
}

#[tokio::test]
async fn missing_permission_never_writes_and_unavailable_is_a_tool_error() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    h.provider.push(Script::call(
        "denied",
        "fs_write",
        json!({"path":"new.txt","content":"secret"}),
    ));
    h.provider.push(Script::reply("no permission"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "write").await;
    done(&mut rx).await;
    assert!(!h.dir.path().join("work/new.txt").exists());
    h.hands_unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    h.provider.push(Script::call(
        "offline",
        "fs_read",
        json!({"path":"new.txt"}),
    ));
    h.provider.push(Script::reply("machine offline"));
    append(&h, chat, "read").await;
    done(&mut rx).await;
    assert!(events(&h,chat).await.iter().any(|e| matches!(&e.payload, Payload::ToolResult(r) if r.is_error && format!("{:?}",r.content).contains("is unreachable"))));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_components_cannot_escape_workdir_or_endpoint_roots() {
    let h = strict().await;
    let chat = working_chat(&h).await;
    grant(&h, chat).await;
    let outside = h.dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "outside-secret").unwrap();
    std::os::unix::fs::symlink(&outside, h.dir.path().join("work/link")).unwrap();
    h.provider.push(Script::call(
        "read-link",
        "fs_read",
        json!({"path":"link/secret.txt"}),
    ));
    h.provider.push(Script::call(
        "write-link",
        "fs_write",
        json!({"path":"link/new.txt","content":"bad"}),
    ));
    h.provider.push(Script::reply("blocked"));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "read linked directory").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    assert!(!outside.join("new.txt").exists());
    let log = events(&h, chat).await;
    let results: Vec<_> = log
        .iter()
        .filter_map(|e| match &e.payload {
            Payload::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(
        |result| result.is_error && format!("{:?}", result.content).contains("symbolic links")
    ));
    assert!(!format!("{log:?}").contains("outside-secret"));
}

#[tokio::test]
async fn cancellation_closes_permission_and_late_allow_creates_no_grant() {
    use eva_protocol::types::{
        module::ModuleKind,
        turn::{Answer, FeedEvent, ItemBody, Permission},
    };
    let h = strict().await;
    let chat = working_chat(&h).await;
    h.live_session(common::CLIENT, ModuleKind::Client, true);
    h.provider.push(Script::call(
        "request-write",
        "fs_write",
        json!({"path":"new.txt","content":"must not write"}),
    ));
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    append(&h, chat, "write with permission").await;
    let pending = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let FeedEvent::ItemStarted { item } = rx.recv().await.unwrap().item.event
                && let ItemBody::ClientOp { pending, .. } = item.body
            {
                break pending;
            }
        }
    })
    .await
    .unwrap();
    let mut session = h.client_session().await;
    session.turns().cancel(chat).await.unwrap();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Cancelled);
    let late = session
        .turns()
        .respond(
            pending,
            Answer::Permission {
                decision: Permission::AllowAlways,
            },
            None,
        )
        .await;
    assert!(late.is_err());
    assert!(!h.dir.path().join("work/new.txt").exists());
    let grants = h
        .soul
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(|c| c.hands_grants().list(&Default::default()))
        .await
        .unwrap();
    assert!(grants.is_empty());
}
