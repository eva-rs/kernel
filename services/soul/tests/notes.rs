//! `notes.list` (`04 §1`, `§2`, `§5`): порядок секции, режим без пересчёта,
//! бюджет от окна модели чата, `tokens` калиброванным ratio, матрица `Policy`.

mod common;

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;

use eva_protocol::{
    errors::Generic,
    requests::Notes::list,
    types::{ChatId, note::Mode},
};
use eva_soul::db::{Db as _, DbResult};

async fn exec(h: &Harness, sql: String) {
    h.soul
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
        .soul
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
        .list(chat)
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
        squeeze_turns,
        tokens: got,
        budget,
    } = list(&h, c).await;
    assert_eq!(squeeze_turns, 0);
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
        cfg.notes.budget_percent = eva_soul::config::notes::BudgetPercent::POS2;
        cfg.notes.budget_max_tokens = 100_000;
    })
    .await;
    let own = chat(&h, "local", Some(common::MODEL)).await;
    assert_eq!(list(&h, own).await.budget, 4000, "2% of the 200k window");

    let h = common::start_with(|cfg| cfg.notes.enabled = false).await;
    let own = chat(&h, "local", None).await;
    let res = h.client_session().await.notes().list(own).await;
    assert_eq!(outcome(&res), Outcome::Unavailable, "{res:?}");
}

#[tokio::test]
async fn unknown_chat_is_not_found() {
    let h = common::start().await;
    let res = h.client_session().await.notes().list(ChatId(999)).await;
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
        let got = outcome(&h.session(caller).await.notes().list(own).await);
        if got != want {
            failures.push(format!("notes.list as {who}: want {want:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `04 §9`: законная заметка обязана помещаться в булавку — иначе soul не стартует.
#[tokio::test]
#[should_panic(
    expected = "notes.pinned_budget_tokens (10) must be at least notes.max_note_tokens (600)"
)]
async fn pinned_budget_below_a_note_refuses_to_start() {
    common::start_with(|cfg| cfg.notes.pinned_budget_tokens = 10).await;
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let own = chat(&h, "local", None).await;
    h.db_fail.store(true, std::sync::atomic::Ordering::Relaxed);
    let res = h.client_session().await.notes().list(own).await;
    assert_eq!(outcome(&res), Outcome::Db, "{res:?}");
}

// ---------------------------------------------------------------------------
// Тулы блокнота и автомат режимов (`04 §5`, `§7`, `§9`): отказ вместо
// обрезания, guard `index`, булавки, переход только на входе в тур.
// ---------------------------------------------------------------------------

mod tools_and_modes {
    use std::collections::BTreeSet;

    use eva::time::Clock as _;

    use eva_protocol::types::{
        ChatId,
        note::{
            Fill, Mode, Op,
            tools::{delete, read, save, update},
        },
    };
    use eva_soul::{
        config::NotesConfig,
        db::{Db as _, DbError, Queries as _, notes::NoteRow},
        subsystems::notes::{
            self, budget, state,
            tools::{self as nt, Applied, Ctx, Error, Rejected},
        },
    };

    use crate::common::{self, Harness};

    /// Тул над писателем soul: `read_this_turn` и режим живут в тесте, как в туре.
    struct Turn {
        chat: ChatId,
        mode: Mode,
        read: BTreeSet<String>,
        budget: u32,
        /// Конфиг тула вместо конфига soul: так достижимы пороги, которые старт soul отвергает.
        cfg: Option<NotesConfig>,
    }

    impl Turn {
        fn new(chat: ChatId) -> Self {
            Self {
                chat,
                mode: Mode::Inline,
                read: BTreeSet::new(),
                budget: 1000,
                cfg: None,
            }
        }

        async fn run<T: Send + 'static>(
            &mut self,
            h: &Harness,
            f: impl FnOnce(&rusqlite::Connection, &NotesConfig, &mut Ctx<'_>) -> Result<T, Error>
            + Send
            + 'static,
        ) -> Result<T, Rejected> {
            let (chat, mode, budget, now) = (self.chat, self.mode, self.budget, h.clock.get());
            let mut read = std::mem::take(&mut self.read);
            let cfg = self
                .cfg
                .clone()
                .unwrap_or_else(|| h.soul.shared().cfg().notes.clone());
            let (res, read) = h
                .soul
                .shared()
                .db()
                .write(move |tx| -> Result<_, DbError> {
                    let mut ctx = Ctx {
                        chat,
                        mode,
                        read_this_turn: &mut read,
                        ratio: None,
                        budget,
                        now,
                    };
                    let res = f(tx, &cfg, &mut ctx);
                    Ok((res, read))
                })
                .await
                .expect("write");
            self.read = read;
            match res {
                Ok(v) => Ok(v),
                Err(Error::Rejected(r)) => Err(r),
                Err(Error::Db(e)) => panic!("db: {e}"),
            }
        }

        async fn save(
            &mut self,
            h: &Harness,
            name: &str,
            body: &str,
            pinned: Option<bool>,
        ) -> Result<Applied<save::Ok>, Rejected> {
            let args = save::Args {
                name: name.to_owned(),
                description: format!("about {}", name.trim().to_lowercase()),
                body: body.to_owned(),
                pinned,
            };
            self.run(h, move |c, cfg, ctx| nt::save(c, cfg, ctx, args))
                .await
        }

        async fn update(
            &mut self,
            h: &Harness,
            args: update::Args,
        ) -> Result<Applied<update::Ok>, Rejected> {
            self.run(h, move |c, cfg, ctx| nt::update(c, cfg, ctx, args))
                .await
        }

        async fn delete(
            &mut self,
            h: &Harness,
            name: &str,
        ) -> Result<Applied<delete::Ok>, Rejected> {
            let args = delete::Args {
                name: name.to_owned(),
            };
            self.run(h, move |c, cfg, ctx| nt::delete(c, cfg, ctx, args))
                .await
        }

        async fn read(&mut self, h: &Harness, names: &[&str]) -> Result<read::Ok, Rejected> {
            let args = read::Args {
                names: names.iter().map(|n| (*n).to_owned()).collect(),
            };
            self.run(h, move |c, cfg, ctx| nt::read(c, cfg, ctx, args))
                .await
        }
    }

    async fn rows(h: &Harness, chat: ChatId) -> Vec<NoteRow> {
        h.query(move |c| c.notes().list(chat)).await
    }

    async fn chat(h: &Harness) -> ChatId {
        super::chat(h, "local", None).await
    }

    /// Вход в тур (`04 §5` п.1): наблюдение, автомат и запись состояния в одной транзакции.
    async fn enter(h: &Harness, c: ChatId, master: bool) -> state::Entered {
        let cfg = h.soul.shared().cfg().notes.clone();
        let now = h.clock.get();
        h.soul
            .shared()
            .db()
            .write(move |tx| -> Result<state::Entered, DbError> {
                let (_, fill) = notes::observe(tx, c, 1000, None)?;
                let entered = state::enter(tx.notes().state(c)?, fill, master, &cfg, now);
                tx.notes().save_state(c, &entered.state)?;
                Ok(entered)
            })
            .await
            .expect("enter")
    }

    #[tokio::test]
    async fn save_update_delete_flow() {
        let h = common::start().await;
        let c = chat(&h).await;
        let mut t = Turn::new(c);

        let saved = t.save(&h, " Plan ", "step 1", None).await.expect("save");
        assert_eq!(saved.ok.name, "plan", "normalized");
        assert!(!saved.ok.replaced);
        assert_eq!(saved.ok.fill.count, 1);
        assert_eq!(saved.event.op, Op::Saved);
        assert_eq!(saved.event.entry, "## plan | about plan\nstep 1");
        assert!(t.read.contains("plan"), "a written name counts as read");

        h.clock.clone().advance(std::time::Duration::from_secs(10));
        let replaced = t.save(&h, "plan", "step 2", None).await.expect("replace");
        assert!(replaced.ok.replaced);
        let row = &rows(&h, c).await[0];
        assert_eq!(
            (row.body.as_str(), row.prev_body.as_deref(), row.pinned),
            ("step 2", Some("step 1"), false)
        );
        assert_ne!(row.created_at, row.updated_at, "created_at is kept");

        let updated = t
            .update(
                &h,
                update::Args {
                    name: String::from("plan"),
                    description: None,
                    body: None,
                    pinned: Some(true),
                },
            )
            .await
            .expect("pin");
        assert_eq!(updated.ok.fields, ["pinned"]);
        assert_eq!(updated.event.op, Op::Updated);
        assert!(rows(&h, c).await[0].pinned);
        let res = t
            .update(
                &h,
                update::Args {
                    name: String::from("plan"),
                    description: None,
                    body: None,
                    pinned: None,
                },
            )
            .await;
        assert!(
            matches!(
                res,
                Err(Rejected {
                    reason: "empty_update",
                    ..
                })
            ),
            "{res:?}"
        );

        let deleted = t.delete(&h, "plan").await.expect("delete");
        assert_eq!(
            (
                deleted.event.op,
                deleted.event.entry.as_str(),
                deleted.ok.fill.count
            ),
            (Op::Deleted, "", 0)
        );
        assert!(rows(&h, c).await.is_empty());
        let res = t.delete(&h, "plan").await;
        assert!(
            matches!(
                res,
                Err(Rejected {
                    reason: "not_found",
                    ..
                })
            ),
            "{res:?}"
        );
        // Удалённое имя оживает как новая заметка.
        let again = t.save(&h, "plan", "fresh", None).await.expect("revive");
        assert!(!again.ok.replaced);
        assert_eq!(rows(&h, c).await[0].prev_body, None);

        // notes.list видит то же.
        let listed = h
            .client_session()
            .await
            .notes()
            .list(c)
            .await
            .expect("list");
        assert_eq!(listed.notes.len(), 1);
        assert_eq!(listed.tokens, again.ok.fill.tokens);
    }

    #[tokio::test]
    async fn refusals_carry_numbers_and_leave_the_db_untouched() {
        let h = common::start_with(|cfg| {
            cfg.notes.max_note_tokens = 10;
            cfg.notes.max_notes = 2;
        })
        .await;
        let c = chat(&h).await;
        let mut t = Turn::new(c);
        t.save(&h, "a", "body a", None).await.expect("a");
        let before = rows(&h, c).await;

        let cases: Vec<(save::Args, &str, &str)> = vec![
            (
                save::Args {
                    name: String::from("Bad Name"),
                    description: String::from("d"),
                    body: String::from("x"),
                    pinned: None,
                },
                "name",
                "name must match [a-z0-9_-]{1,48}",
            ),
            (
                save::Args {
                    name: String::from("b"),
                    description: String::new(),
                    body: String::from("x"),
                    pinned: None,
                },
                "description",
                "description: 1..120 chars, got 0",
            ),
            (
                save::Args {
                    name: String::from("b"),
                    description: "d".repeat(121),
                    body: String::from("x"),
                    pinned: None,
                },
                "description",
                "description: 1..120 chars, got 121",
            ),
            (
                save::Args {
                    name: String::from("b"),
                    description: String::from("d"),
                    body: String::from("  "),
                    pinned: None,
                },
                "empty_body",
                "empty body; to remove a note use notes_delete",
            ),
            (
                save::Args {
                    name: String::from("b"),
                    description: String::from("d"),
                    body: "x".repeat(100),
                    pinned: None,
                },
                "body_tokens",
                "body is 25 tokens, limit 10; split into two notes or cut",
            ),
        ];
        for (args, reason, message) in cases {
            let res = t
                .run(&h, move |c, cfg, ctx| nt::save(c, cfg, ctx, args))
                .await;
            assert_eq!(
                res.err(),
                Some(Rejected {
                    reason,
                    message: message.to_owned()
                })
            );
        }
        t.save(&h, "b", "body b", None).await.expect("second fits");
        let res = t.save(&h, "c", "body c", None).await;
        assert_eq!(
            res.err(),
            Some(Rejected {
                reason: "max_notes",
                message: String::from("notebook holds 2 notes; delete or merge first")
            })
        );
        let after = rows(&h, c).await;
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], before[0], "refusals change nothing");
    }

    #[tokio::test]
    async fn pinned_refusals_distinguish_three_causes() {
        // 40 байт = 10 токенов.
        let body = "x".repeat(40);
        let h2 = common::start().await;
        let c2 = chat(&h2).await;
        let mut t2 = Turn::new(c2);
        // `pinned_body` достижима только при бюджете булавок ниже потолка тела —
        // с таким конфигом soul не стартует, поэтому он подан тулу напрямую.
        t2.cfg = Some(NotesConfig {
            pinned_max: 2,
            pinned_budget_tokens: 20,
            max_note_tokens: 600,
            ..NotesConfig::default()
        });
        t2.save(&h2, "one", &body, Some(true)).await.expect("one");
        let res = t2.save(&h2, "big", &"y".repeat(100), Some(true)).await;
        assert_eq!(
            res.err(),
            Some(Rejected {
                reason: "pinned_body",
                message: String::from("note is 25 tokens, pinned budget 20; cut it")
            })
        );
        let res = t2.save(&h2, "two", &"z".repeat(44), Some(true)).await;
        assert_eq!(
            res.err(),
            Some(Rejected {
                reason: "pinned_sum",
                message: String::from("pinned budget 20 tokens: one already take 10; unpin one")
            })
        );
        t2.save(&h2, "two", &"z".repeat(36), Some(true))
            .await
            .expect("two fits");
        let res = t2.save(&h2, "three", "q", Some(true)).await;
        assert_eq!(
            res.err(),
            Some(Rejected {
                reason: "pinned_max",
                message: String::from("2 notes already pinned: one, two")
            })
        );
        // Булавка сверх бюджета не пропадает из index-рендера.
        let cfg = NotesConfig {
            pinned_budget_tokens: 10,
            ..h2.soul.shared().cfg().notes.clone()
        };
        let section = notes::render::index(&rows(&h2, c2).await, &cfg, None);
        assert!(
            section.contains("## one | about one\n")
                && section.contains("body NOT quoted — notes_read it"),
            "{section}"
        );
    }

    #[tokio::test]
    async fn index_guard_requires_a_read_in_the_same_turn() {
        let h = common::start_with(|cfg| cfg.notes.index_max_tokens = 25).await;
        let c = chat(&h).await;
        let mut t = Turn::new(c);
        t.save(&h, "job", "state: step 1", None)
            .await
            .expect("inline save");

        t.mode = Mode::Index;
        t.read.clear();
        let res = t.save(&h, "job", "state: step 2", None).await;
        assert_eq!(
            res.err(),
            Some(Rejected {
                reason: "guard",
                message: String::from("read job with notes_read first, then rewrite it whole")
            })
        );
        let res = t
            .update(
                &h,
                update::Args {
                    name: String::from("job"),
                    description: None,
                    body: Some(String::from("x")),
                    pinned: None,
                },
            )
            .await;
        assert_eq!(res.err().map(|r| r.reason), Some("guard"));
        let read = t.read(&h, &["JOB", "nope"]).await.expect("read");
        assert_eq!(
            read.text,
            "## job | about job\nstate: step 1\n\n- nope: not found"
        );
        let saved = t
            .save(&h, "job", "state: step 2", None)
            .await
            .expect("after read");
        assert_eq!(
            saved.event.entry, "- job — about job",
            "index entry is a line"
        );
        // Новая заметка guard не требует, но стена индекса — да.
        t.read.clear();
        let res = t.save(&h, "another-rather-long-note-name", "x", None).await;
        assert_eq!(
            res.as_ref().err().map(|r| r.reason),
            Some("index_tokens"),
            "{res:?}"
        );
        let res = t.read(&h, &[]).await;
        assert_eq!(res.err().map(|r| r.reason), Some("names"));

        // В inline guard не действует.
        t.mode = Mode::Inline;
        t.read.clear();
        t.save(&h, "job", "state: step 3", None)
            .await
            .expect("inline rewrite without read");
    }

    #[tokio::test]
    async fn mode_changes_only_on_turn_entry_and_list_reads_it() {
        let h = common::start_with(|cfg| cfg.notes.inline_max_notes = 2).await;
        let c = chat(&h).await;
        let mut t = Turn::new(c);
        for name in ["a", "b", "c"] {
            t.save(&h, name, "body", None).await.expect("save");
        }
        // Три мутации подряд: notes_state не появилась.
        assert!(h.query(move |q| q.notes().state(c)).await.is_none());

        let e = enter(&h, c, true).await;
        assert_eq!(
            e.transition,
            Some((Mode::Inline, Mode::Squeeze)),
            "3 > inline_max_notes"
        );
        let e = enter(&h, c, true).await;
        assert_eq!((e.transition, e.state.squeeze_turns), (None, 1));
        let e = enter(&h, c, true).await;
        assert_eq!(e.transition, Some((Mode::Squeeze, Mode::Index)));
        assert_eq!(
            notes::transition_key(Mode::Squeeze, Mode::Index),
            "squeeze→index"
        );

        let listed = h
            .client_session()
            .await
            .notes()
            .list(c)
            .await
            .expect("list");
        assert_eq!((listed.mode, listed.squeeze_turns), (Mode::Index, 0));
        assert_eq!(
            listed.tokens,
            budget::fill(&rows(&h, c).await, 1000, None).tokens
        );
        assert_eq!(
            Fill {
                tokens: 3,
                budget: 10,
                count: 1
            }
            .percent(),
            30
        );
    }
}
