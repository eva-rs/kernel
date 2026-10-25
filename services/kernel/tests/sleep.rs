//! `sleep.*` (`08 §2.8`): журнал ночей keyset'ом, фазы в порядке и с
//! `ops_json` целиком, матрица `Policy`.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, service::Sleep::Trait as _};
use eva_kernel::{
    db::{Db as _, DbResult},
    subsystems::{
        SleepTurns, WarmSets,
        sleep::{
            Outcome as SleepOutcome, PhaseTurn, Sleep, Tick, WarmOutcome,
            memory::{self, ToolResult},
        },
    },
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        RunsLimit,
        sleep::{phase_runs, runs},
    },
    types::{
        MemoryId, RunId,
        sleep::{Phase, PhaseOp, Verdict},
    },
    wire::events::{Event, PhaseState},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

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

fn t0(h: &Harness) -> i64 {
    i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit")
}

fn ops() -> Vec<PhaseOp> {
    vec![
        PhaseOp {
            tool: String::from("sleep_memory_forget"),
            input: serde_json::json!({"id": 4, "why": "stale"}),
            verdict: Verdict::Applied,
        },
        PhaseOp {
            tool: String::from("sleep_memory_merge"),
            input: serde_json::json!({"ids": [1, 2]}),
            verdict: Verdict::Discarded {
                reason: String::from("untrusted source"),
            },
        },
    ]
}

/// Ночи `n1..n4` (`n2`/`n3` стартовали одновременно); у `n4` три фазы,
/// вставленные не по порядку.
async fn seed(h: &Harness) {
    let t = t0(h);
    let ops = serde_json::to_string(&ops()).expect("ops json");
    exec(
        h,
        format!(
            "INSERT INTO sleep_runs (run_id, started, finished, ok, dry_run, interrupted, report) VALUES
               ('n1', {t1}, {t1}, 1, 0, 0, 'night one'),
               ('n2', {t2}, {t2}, 0, 1, 0, NULL),
               ('n3', {t2}, NULL, NULL, 0, 1, NULL),
               ('n4', {t4}, {t4}, 1, 0, 0, 'applied 1');
             INSERT INTO sleep_phase_runs (run_id, phase, started, finished, model, ops_json, applied, discarded, report) VALUES
               ('n4', 'warm', {t4}, {t4}, NULL, '[]', 0, 0, NULL),
               ('n4', 'memory', {t4}, {t4}, 'mock-model', '{ops}', 1, 1, 'memory done'),
               ('n4', 'groups', {t4}, NULL, NULL, '[]', 0, 0, '[interrupted]');",
            t1 = t + SEC,
            t2 = t + 2 * SEC,
            t4 = t + 4 * SEC,
        ),
    )
    .await;
}

async fn page(
    h: &Harness,
    limit: u16,
    after: Option<&str>,
) -> Result<Vec<String>, Generic<runs::Err>> {
    let runs::Ok { found } = h
        .client_session()
        .await
        .sleep()
        .runs(
            RunsLimit::new(limit).expect("limit in range"),
            after.map(RunId::from),
        )
        .await?;
    Ok(found.into_iter().map(|r| r.run_id.0).collect())
}

#[tokio::test]
async fn runs_newest_first_with_keyset_pages() {
    let h = common::start().await;
    seed(&h).await;

    let all = page(&h, 20, None).await.expect("all");
    assert_eq!(all, ["n4", "n3", "n2", "n1"]);

    for limit in [1, 2, 3] {
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let found = page(&h, limit, after.as_deref()).await.expect("page");
            seen.extend(found.iter().cloned());
            if found.len() < usize::from(limit) {
                break;
            }
            after = found.last().cloned();
        }
        assert_eq!(seen, all, "limit {limit}: pages neither repeat nor skip");
    }

    let res = page(&h, 20, Some("nope")).await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::NotFound(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn runs_carry_flags() {
    let h = common::start().await;
    seed(&h).await;
    let runs::Ok { found } = h
        .client_session()
        .await
        .sleep()
        .inner_mut()
        .runs()
        .call(runs::Args::default())
        .await
        .expect("runs");
    let n3 = found
        .iter()
        .find(|r| r.run_id.as_str() == "n3")
        .expect("n3");
    assert!(n3.interrupted && !n3.dry_run && n3.finished.is_none() && n3.ok.is_none());
    let n2 = found
        .iter()
        .find(|r| r.run_id.as_str() == "n2")
        .expect("n2");
    assert!(n2.dry_run && n2.ok == Some(false));
    assert_eq!(
        found.last().and_then(|r| r.report.as_deref()),
        Some("night one")
    );
}

#[tokio::test]
async fn phase_runs_in_phase_order_with_ops() {
    let h = common::start().await;
    seed(&h).await;
    let mut session = h.client_session().await;

    let phase_runs::Ok { phases } = session
        .sleep()
        .phase_runs(RunId::from("n4"))
        .await
        .expect("phases");
    let order: Vec<_> = phases.iter().map(|p| p.phase).collect();
    assert_eq!(order, [Phase::Memory, Phase::Groups, Phase::Warm]);
    assert_eq!(phases[0].ops, ops());
    assert_eq!((phases[0].applied, phases[0].discarded), (1, 1));
    assert_eq!(
        phases[0].model.as_ref().map(|m| m.as_str()),
        Some(common::MODEL)
    );
    assert_eq!(phases[1].report.as_deref(), Some("[interrupted]"));
    assert!(phases[1].finished.is_none());

    let phase_runs::Ok { phases } = session
        .sleep()
        .phase_runs(RunId::from("n1"))
        .await
        .expect("night without phases");
    assert!(phases.is_empty());

    let res = session.sleep().phase_runs(RunId::from("nope")).await;
    assert!(
        matches!(res, Err(Generic::Spec(phase_runs::Err::RunNotFound(ref e))) if e.run.as_str() == "nope"),
        "{res:?}"
    );
}

#[tokio::test]
async fn corrupt_ops_json_is_a_db_error() {
    let h = common::start().await;
    seed(&h).await;
    exec(
        &h,
        String::from("UPDATE sleep_phase_runs SET ops_json = '{' WHERE phase = 'warm';"),
    )
    .await;
    let res = h
        .client_session()
        .await
        .sleep()
        .phase_runs(RunId::from("n4"))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}

#[tokio::test]
async fn sleep_matrix() {
    use Outcome::{Forbidden as F, Ok as K};

    let h = common::start().await;
    seed(&h).await;
    h.service_with_surface(common::SVC).await;
    h.service_with_surface(common::OTHER).await;

    let mut failures = Vec::new();
    for ((who, caller), want) in common::matrix_callers().into_iter().zip([K, K, F, F]) {
        let mut s = h.session(caller).await;
        let runs = outcome(
            &s.sleep()
                .inner_mut()
                .runs()
                .call(runs::Args::default())
                .await,
        );
        let phases = outcome(&s.sleep().phase_runs(RunId::from("n4")).await);
        for (op, got) in [("runs", runs), ("phase_runs", phases)] {
            if got != want {
                failures.push(format!("sleep.{op} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- компонент `sleep` (`08 §2`) ---

const DAY: i64 = 86_400 * SEC;

/// Строка `memory`: возраст — дни назад от часов теста, `consolidated_at` пуст.
fn mem(
    t: i64,
    id: i64,
    (kind, vis): (&str, &str),
    text: &str,
    importance: f64,
    age_days: i64,
    trusted: bool,
) -> String {
    let at = t - age_days * DAY;
    format!(
        "INSERT INTO memory (id, kind, text, visibility, importance, access_count, source_trusted_chat,
                             created_at, updated_at, valid_from, last_reinforced_at, version)
           VALUES ({id}, '{kind}', '{text}', '{vis}', {importance}, 0, {trusted}, {at}, {at}, {at}, {at}, 1);",
        trusted = i32::from(trusted),
    )
}

/// Кластеры `{1,9}` (core и его тёзка), `{2,3,4}` (чай; 4 из публичного чата),
/// `{7,8}` (кот; 7 моложе `keep_recent`) и forget-кандидаты `5`, `6`.
async fn seed_corpus(h: &Harness) {
    let t = t0(h);
    let sql = [
        mem(
            t,
            1,
            ("core", "trusted"),
            "eva speaks russian with the master at home",
            0.9,
            100,
            true,
        ),
        mem(
            t,
            9,
            ("fact", "private"),
            "eva speaks russian with the master at work",
            0.5,
            100,
            true,
        ),
        mem(
            t,
            2,
            ("fact", "private"),
            "master likes green tea in the morning",
            0.5,
            100,
            true,
        ),
        mem(
            t,
            3,
            ("fact", "private"),
            "master likes green tea every morning",
            0.5,
            100,
            true,
        ),
        mem(
            t,
            4,
            ("fact", "public"),
            "master likes green tea in the evening too",
            0.5,
            100,
            false,
        ),
        mem(
            t,
            5,
            ("event", "private"),
            "went to the dentist last spring",
            0.1,
            100,
            true,
        ),
        mem(
            t,
            6,
            ("event", "private"),
            "bought a new keyboard in november",
            0.2,
            100,
            true,
        ),
        mem(
            t,
            7,
            ("fact", "private"),
            "the cat is named mochi and sleeps all day",
            0.5,
            1,
            true,
        ),
        mem(
            t,
            8,
            ("fact", "private"),
            "the cat is named mochi and sleeps all night",
            0.5,
            100,
            true,
        ),
    ]
    .concat();
    exec(h, sql).await;
}

async fn start_sleepy(tweak: impl FnOnce(&mut eva_kernel::Config)) -> Harness {
    common::start_with(|cfg| {
        cfg.system.tz = Some(String::from("UTC"));
        cfg.sleep.min_memories = 2;
        cfg.sleep.max_forget = 1;
        tweak(cfg);
    })
    .await
}

fn reason(r: &ToolResult) -> Option<&str> {
    r.is_error.then(|| r.output["error"].as_str()).flatten()
}

async fn rows(h: &Harness, sql: &'static str) -> Vec<String> {
    h.query(move |c| {
        let mut stmt = c.prepare(sql)?;
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..n)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|cols| cols.join("|"))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

#[tokio::test]
async fn vet_rejects_by_the_table_and_applies_the_rest() {
    let h = start_sleepy(|_| {}).await;
    seed_corpus(&h).await;
    let shared = h.kernel.shared();
    let run = RunId::from("night-1");
    let now = shared.now();
    let mut phase = memory::Phase::build(shared, run.clone(), now)
        .await
        .expect("build")
        .expect("corpus is big enough");
    let id = MemoryId;
    assert_eq!(
        phase.snapshot().clusters,
        [
            vec![id(1), id(9)],
            vec![id(2), id(3), id(4)],
            vec![id(7), id(8)]
        ]
    );
    assert_eq!(
        phase
            .snapshot()
            .candidates
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [id(5), id(6)]
    );
    assert!(
        phase
            .prompt()
            .starts_with("[sleep memory phase]\ncorpus: 9 current")
    );

    let calls = [
        (
            "sleep_memory_forget",
            json!({"id": 5, "why": " "}),
            Some(memory::WHY_REQUIRED),
        ),
        (
            "sleep_memory_forget",
            json!({"id": 99, "why": "x"}),
            Some(memory::ID_NOT_SHOWN),
        ),
        (
            "sleep_memory_forget",
            json!({"id": 2, "why": "x"}),
            Some(memory::NOT_CANDIDATE),
        ),
        (
            "sleep_memory_merge",
            json!({"ids": [1, 9], "text": "eva speaks russian", "why": "dup"}),
            Some(memory::CORE_NEVER_DELETED),
        ),
        (
            "sleep_memory_merge",
            json!({"ids": [7, 8], "text": "the cat mochi sleeps", "why": "dup"}),
            Some(memory::TOO_RECENT),
        ),
        (
            "sleep_memory_change_type",
            json!({"id": 4, "kind": "core", "why": "matters"}),
            Some(memory::UNTRUSTED),
        ),
        (
            "sleep_memory_change_type",
            json!({"id": 2, "kind": "procedure", "why": "a habit"}),
            None,
        ),
        (
            "sleep_memory_demote",
            json!({"id": 1, "why": "too specific"}),
            None,
        ),
        ("sleep_memory_forget", json!({"id": 5, "why": "old"}), None),
        (
            "sleep_memory_forget",
            json!({"id": 6, "why": "old"}),
            Some(memory::CAP_REACHED),
        ),
        (
            "sleep_memory_merge",
            json!({"ids": [1, 9], "text": "eva speaks russian with the master", "why": "dup"}),
            None,
        ),
        (
            "sleep_memory_split",
            json!({"id": 8, "into": [{"text": "the cat is mochi"}], "why": "x"}),
            Some("into: at least two parts"),
        ),
    ];
    for (tool, input, want) in calls {
        let got = phase.call(tool, input.clone()).await;
        assert_eq!(reason(&got), want, "{tool} {input}");
    }
    // Хозяин правит запись посреди фазы: операция сна над ней — stale, правка цела.
    exec(
        &h,
        String::from("UPDATE memory SET text = 'master likes matcha', version = 2 WHERE id = 3;"),
    )
    .await;
    let stale = phase
        .call(
            "sleep_memory_supersede",
            json!({"id": 3, "text": "master drinks tea", "why": "x"}),
        )
        .await;
    assert_eq!(reason(&stale), Some(memory::STALE));

    let summary = phase.finish(true).await.expect("finish");
    assert_eq!(
        (summary.applied, summary.discarded, summary.forgets),
        (4, 9, 1)
    );
    let ops: Vec<PhaseOp> = serde_json::from_str(&summary.ops_json).expect("ops_json");
    assert_eq!(ops.len(), 13);
    assert!(
        matches!(&ops[0].verdict, Verdict::Discarded { reason } if reason == memory::WHY_REQUIRED)
    );
    assert_eq!(ops[6].verdict, Verdict::Applied);

    assert_eq!(
        rows(&h, "SELECT id, kind, visibility, valid_to IS NULL, version FROM memory WHERE id IN (1, 2, 3, 9) ORDER BY id").await,
        [
            "Integer(1)|Text(\"fact\")|Text(\"private\")|Integer(1)|Integer(3)",
            "Integer(2)|Text(\"procedure\")|Text(\"private\")|Integer(1)|Integer(2)",
            "Integer(3)|Text(\"fact\")|Text(\"private\")|Integer(1)|Integer(2)",
            "Integer(9)|Text(\"fact\")|Text(\"private\")|Integer(0)|Integer(1)",
        ],
        "demote then merge into 1 with the strictest visibility; 3 keeps the master's edit"
    );
    assert_eq!(
        rows(
            &h,
            "SELECT memory_id, reason, run_id FROM memory_archive ORDER BY memory_id"
        )
        .await,
        [
            "Integer(5)|Text(\"forget\")|Text(\"night-1\")",
            "Integer(9)|Text(\"merge\")|Text(\"night-1\")",
        ]
    );
    assert!(
        rows(
            &h,
            "SELECT id FROM memory WHERE kind = 'core' AND valid_to IS NOT NULL"
        )
        .await
        .is_empty()
    );
    assert_eq!(
        rows(
            &h,
            "SELECT id FROM memory WHERE valid_to IS NULL AND consolidated_at IS NULL ORDER BY id"
        )
        .await,
        Vec::<String>::new(),
        "every shown record is consolidated"
    );
    assert_eq!(h.counter("sleep.ops_applied").await, 4);
    assert_eq!(h.counter("sleep.ops_discarded").await, 9);
}

#[tokio::test]
async fn dry_run_writes_nothing_but_the_journal() {
    let h = start_sleepy(|cfg| cfg.sleep.dry_run = true).await;
    seed_corpus(&h).await;
    const DUMP: &str = "SELECT * FROM memory ORDER BY id";
    let before = rows(&h, DUMP).await;
    let shared = h.kernel.shared();
    let mut phase = memory::Phase::build(shared, RunId::from("dry"), shared.now())
        .await
        .expect("build")
        .expect("corpus");

    let forget = phase
        .call("sleep_memory_forget", json!({"id": 5, "why": "old"}))
        .await;
    assert!(!forget.is_error);
    let merge = phase
        .call(
            "sleep_memory_merge",
            json!({"ids": [2, 3], "text": "master likes green tea", "why": "dup"}),
        )
        .await;
    assert!(!merge.is_error);
    let capped = phase
        .call("sleep_memory_forget", json!({"id": 6, "why": "old"}))
        .await;
    assert_eq!(
        reason(&capped),
        Some(memory::CAP_REACHED),
        "caps count would-apply ops too"
    );
    let summary = phase.finish(true).await.expect("finish");

    assert_eq!(rows(&h, DUMP).await, before, "memory is byte-identical");
    assert!(rows(&h, "SELECT * FROM memory_archive").await.is_empty());
    let ops: Vec<PhaseOp> = serde_json::from_str(&summary.ops_json).expect("ops_json");
    assert_eq!(
        ops.iter().map(|o| o.verdict.clone()).collect::<Vec<_>>()[..2],
        [Verdict::WouldApply, Verdict::WouldApply]
    );
}

/// Движок туров теста: фаза 1 исполняет заготовленные вызовы, фаза 2 молчит,
/// warm-набор отвечает числами; всё увиденное — в журнал.
#[derive(Clone, Default)]
struct Script {
    calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    turns: Arc<Mutex<Vec<PhaseTurn>>>,
    clusters: Arc<Mutex<Vec<usize>>>,
    /// Отменить ночь из фазы 1, как это сделал бы `Shutdown`.
    cancel_in_memory: bool,
}

impl<C: eva_kernel::Components> SleepTurns<C> for Script {
    async fn memory(
        &self,
        turn: PhaseTurn,
        phase: Arc<tokio::sync::Mutex<memory::Phase<C>>>,
    ) -> SleepOutcome {
        let mut phase = phase.lock().await;
        self.clusters
            .lock()
            .expect("clusters")
            .push(phase.snapshot().clusters.len());
        if self.cancel_in_memory {
            turn.cancel.cancel();
        }
        self.turns.lock().expect("turns").push(turn);
        let calls = self.calls.lock().expect("calls").clone();
        for (tool, input) in calls {
            phase.call(&tool, input).await;
        }
        SleepOutcome::Done {
            report: String::from("tidied"),
        }
    }

    async fn groups(
        &self,
        turn: PhaseTurn,
        _: Arc<tokio::sync::Mutex<eva_kernel::subsystems::sleep::groups::Phase<C>>>,
    ) -> SleepOutcome {
        self.turns.lock().expect("turns").push(turn);
        SleepOutcome::Done {
            report: String::from("no changes"),
        }
    }
}

impl WarmSets for Script {
    async fn recompute(&self, _: eva::time::Timestamp) -> Result<WarmOutcome, String> {
        Ok(WarmOutcome {
            chats: 1,
            added: 2,
            removed: 0,
        })
    }
}

#[tokio::test]
async fn a_night_runs_three_phases_and_logs_them() {
    let h = start_sleepy(|_| {}).await;
    seed_corpus(&h).await;
    let script = Script::default();
    script.calls.lock().expect("calls").push((
        String::from("sleep_memory_forget"),
        json!({"id": 5, "why": "old"}),
    ));
    let sleep = Sleep::new(h.kernel.clone(), script.clone());
    let mut events = h.kernel.shared().bus().subscribe();
    let cancel = CancellationToken::new();

    let Tick::Slept(run) = sleep.tick(&cancel).await.expect("tick") else {
        panic!("midnight UTC is due and the kernel is quiet");
    };
    assert_eq!(
        sleep.tick(&cancel).await.expect("tick"),
        Tick::Idle,
        "one night per boundary"
    );

    let turns = script.turns.lock().expect("turns").clone();
    assert_eq!(
        turns.iter().map(|t| (t.phase, t.reset)).collect::<Vec<_>>(),
        [(Phase::Memory, true), (Phase::Groups, false)],
        "only the first turn of the night resets the segment"
    );
    assert_eq!(
        turns[0].model.as_str(),
        common::MODEL,
        "no smartest role: the default model"
    );
    let undeletable: bool = h
        .query({
            let chat = turns[0].chat;
            move |c| {
                Ok(c.query_row(
                    "SELECT undeletable FROM chats WHERE id = ?1",
                    [chat.get()],
                    |r| r.get(0),
                )?)
            }
        })
        .await;
    assert!(undeletable, "the sleep chat");

    let phase_runs::Ok { phases } = h
        .client_session()
        .await
        .sleep()
        .phase_runs(run.clone())
        .await
        .expect("phases");
    let reports: Vec<_> = phases
        .iter()
        .map(|p| (p.phase, p.report.clone(), p.finished.is_some()))
        .collect();
    assert_eq!(
        reports,
        [
            (Phase::Memory, Some(String::from("tidied")), true),
            (Phase::Groups, Some(String::from("no changes")), true),
            (
                Phase::Warm,
                Some(String::from("chats 1, added 2, removed 0")),
                true
            ),
        ]
    );
    assert_eq!((phases[0].applied, phases[0].ops.len()), (1, 1));
    assert_eq!(phases[2].model, None, "warm needs no model");
    let night = rows(
        &h,
        "SELECT ok, interrupted, dry_run, report FROM sleep_runs",
    )
    .await;
    assert_eq!(
        night,
        [
            "Integer(1)|Integer(0)|Integer(0)|Text(\"memory: tidied\\ngroups: no changes\\nwarm: chats 1, added 2, removed 0\\napplied 1, discarded 0, forgotten 1\")"
        ]
    );
    assert_eq!(h.counter("sleep.runs").await, 1);

    let mut seen = Vec::new();
    while let Ok(p) = events.try_recv() {
        if let Event::SleepPhase(e) = p.event {
            seen.push((e.phase, e.state));
        }
    }
    assert_eq!(
        seen,
        [
            (Phase::Memory, PhaseState::Started),
            (Phase::Memory, PhaseState::Finished),
            (Phase::Groups, PhaseState::Started),
            (Phase::Groups, PhaseState::Finished),
            (Phase::Warm, PhaseState::Started),
            (Phase::Warm, PhaseState::Finished),
        ]
    );
}

#[tokio::test]
async fn needs_review_is_idempotent_across_nights() {
    let h = start_sleepy(|_| {}).await;
    seed_corpus(&h).await;
    let script = Script::default();
    let sleep = Sleep::new(h.kernel.clone(), script.clone());
    let cancel = CancellationToken::new();

    assert!(matches!(
        sleep.tick(&cancel).await.expect("tick"),
        Tick::Slept(_)
    ));
    let mut clock = h.clock.clone();
    clock.advance(Duration::from_secs(86_400));
    assert!(matches!(
        sleep.tick(&cancel).await.expect("tick"),
        Tick::Slept(_)
    ));

    assert_eq!(
        *script.clusters.lock().expect("clusters"),
        [3, 0],
        "nothing changed, nothing to review"
    );
}

#[tokio::test]
async fn shutdown_mid_night_skips_the_remaining_phases() {
    let h = start_sleepy(|_| {}).await;
    seed_corpus(&h).await;
    let script = Script {
        cancel_in_memory: true,
        ..Script::default()
    };
    let sleep = Sleep::new(h.kernel.clone(), script.clone());

    assert!(matches!(
        sleep.tick(&CancellationToken::new()).await.expect("tick"),
        Tick::Slept(_)
    ));
    assert_eq!(
        rows(&h, "SELECT phase FROM sleep_phase_runs").await,
        ["Text(\"memory\")"]
    );
    assert_eq!(
        rows(&h, "SELECT ok, interrupted FROM sleep_runs").await,
        ["Integer(0)|Integer(1)"]
    );
    assert_eq!(h.counter("sleep.interrupted").await, 1);
}

/// Реплика человека (`sender` неизвестен) в чате 10 в момент `at`.
async fn human(h: &Harness, seq: i64, at: i64) {
    exec(
        h,
        format!(
            "INSERT INTO events (id, chat_id, segment_id, seq, kind, payload, created_at)
               VALUES ({seq}, 10, 1, {seq}, 'message', x'', {at});
             INSERT INTO messages (chat_id, seq, event_id, role, sender_person_id) VALUES (10, {seq}, {seq}, 'user', NULL);"
        ),
    )
    .await;
}

#[tokio::test]
async fn the_night_waits_for_quiet_unless_forced() {
    let h = start_sleepy(|_| {}).await;
    let t = t0(&h);
    exec(
        &h,
        format!(
            "INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
               SELECT 10, id, 'persistent', {t}, {t} FROM folders WHERE surface_id = 'local' AND name = 'Unsorted';
             INSERT INTO segments (id, chat_id, seq, started_at, reason, system_snapshot, tools_snapshot, hot_memory_ids)
               VALUES (1, 10, 1, {t}, 'initial', x'', x'', x'');"
        ),
    )
    .await;
    human(&h, 1, t - 60 * SEC).await;
    let sleep = Sleep::new(h.kernel.clone(), Script::default());
    let cancel = CancellationToken::new();
    let mut clock = h.clock.clone();

    assert_eq!(
        sleep.tick(&cancel).await.expect("tick"),
        Tick::Idle,
        "a human spoke a minute ago"
    );
    clock.advance(Duration::from_secs(8 * 60));
    assert_eq!(
        sleep.tick(&cancel).await.expect("tick"),
        Tick::Idle,
        "nine minutes of silence are not enough"
    );
    clock.advance(Duration::from_secs(60));
    assert!(
        matches!(sleep.tick(&cancel).await.expect("tick"), Tick::Slept(_)),
        "idle_for reached"
    );

    // Следующая ночь (00:01): свежая реплика держит её до `force_after`.
    clock.advance(Duration::from_secs(86_400 - 8 * 60));
    human(&h, 2, t0(&h)).await;
    assert_eq!(sleep.tick(&cancel).await.expect("tick"), Tick::Idle);
    clock.advance(Duration::from_secs(3 * 3600));
    human(&h, 3, t0(&h)).await;
    assert!(
        matches!(sleep.tick(&cancel).await.expect("tick"), Tick::Slept(_)),
        "force_after"
    );
}

#[tokio::test]
async fn a_disabled_or_providerless_night_is_skipped_once() {
    let h = start_sleepy(|cfg| cfg.sleep.enabled = false).await;
    let sleep = Sleep::new(h.kernel.clone(), Script::default());
    let cancel = CancellationToken::new();
    assert_eq!(sleep.tick(&cancel).await.expect("tick"), Tick::Skipped);
    assert_eq!(sleep.tick(&cancel).await.expect("tick"), Tick::Idle);
    assert_eq!(h.counter("sleep.skipped").await, 1);
    assert!(rows(&h, "SELECT run_id FROM sleep_runs").await.is_empty());

    let h = start_sleepy(|cfg| {
        cfg.llm.default_model = eva_protocol::types::ModelId::from("not-served")
    })
    .await;
    let sleep = Sleep::new(h.kernel.clone(), Script::default());
    assert_eq!(
        sleep.tick(&cancel).await.expect("tick"),
        Tick::Idle,
        "waits for the provider"
    );
    let mut clock = h.clock.clone();
    clock.advance(*h.kernel.shared().cfg().llm.provider_wait);
    assert_eq!(sleep.tick(&cancel).await.expect("tick"), Tick::Skipped);
    assert_eq!(sleep.tick(&cancel).await.expect("tick"), Tick::Idle);
}

#[tokio::test]
async fn recover_closes_nights_of_the_previous_process() {
    let h = start_sleepy(|_| {}).await;
    let t = t0(&h);
    exec(
        &h,
        format!(
            "INSERT INTO sleep_runs (run_id, started, dry_run) VALUES ('open', {t}, 0);
             INSERT INTO sleep_phase_runs (run_id, phase, started, finished, report) VALUES
               ('open', 'memory', {t}, {t}, 'done'), ('open', 'groups', {t}, NULL, NULL);"
        ),
    )
    .await;
    let sleep = Sleep::new(h.kernel.clone(), Script::default());
    sleep.recover().await.expect("recover");
    assert_eq!(
        rows(
            &h,
            "SELECT ok, interrupted, finished IS NOT NULL FROM sleep_runs"
        )
        .await,
        ["Integer(0)|Integer(1)|Integer(1)"]
    );
    assert_eq!(
        rows(
            &h,
            "SELECT phase, report FROM sleep_phase_runs ORDER BY phase"
        )
        .await,
        [
            "Text(\"groups\")|Text(\"[interrupted]\")",
            "Text(\"memory\")|Text(\"done\")"
        ]
    );
    assert_eq!(h.counter("sleep.interrupted").await, 1);
}

/// Vet исчерпывающ (`08 §5`): случайный поток операций над случайным корпусом
/// не нарушает ни одного инварианта таблицы.
#[test]
fn vet_holds_over_random_corpora() {
    use proptest::prelude::*;

    const WORDS: [&str; 4] = [
        "master likes green tea in the morning",
        "master likes green tea every evening",
        "the cat sleeps on the warm keyboard",
        "the cat sleeps on the cold keyboard",
    ];
    const KINDS: [&str; 4] = ["fact", "event", "core", "procedure"];
    const VIS: [&str; 3] = ["public", "trusted", "private"];
    const TOOLS: [&str; 6] = [
        "forget",
        "merge",
        "split",
        "change_type",
        "supersede",
        "demote",
    ];

    let row = (
        0..4usize,
        0..4usize,
        0..3usize,
        0..=10u32,
        any::<bool>(),
        any::<bool>(),
    );
    let op = (0..6usize, 1..=9i64, 1..=9i64, 0..4usize, any::<bool>());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let h = rt.block_on(start_sleepy(|cfg| {
        cfg.sleep.max_ops = 6;
        cfg.sleep.max_forget = 2;
    }));
    let night = std::sync::atomic::AtomicU32::new(0);

    proptest!(ProptestConfig::with_cases(24), |(
        corpus in prop::collection::vec(row, 2..9),
        ops in prop::collection::vec(op, 0..16),
    )| {
        let t = t0(&h);
        let mut sql = String::from("DELETE FROM memory_link; DELETE FROM memory_archive; DELETE FROM memory;");
        for (i, (text, kind, vis, importance, old, trusted)) in corpus.iter().enumerate() {
            let id = i64::try_from(i).expect("small") + 1;
            let age = if *old { 400 } else { 1 };
            sql += &mem(t, id, (KINDS[*kind], VIS[*vis]), &format!("{} {id}", WORDS[*text]), f64::from(*importance) / 10.0, age, *trusted);
        }
        rt.block_on(exec(&h, sql));
        let run = RunId(format!("night-{}", night.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let before: Vec<(i64, String)> = rt.block_on(h.query(|c| {
            let mut stmt = c.prepare("SELECT id, visibility FROM memory ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            Ok(rows)
        }));

        let summary = rt.block_on(async {
            let shared = h.kernel.shared();
            let mut phase = memory::Phase::build(shared, run.clone(), shared.now())
                .await
                .expect("build")?;
            for (tool, a, b, kind, why) in &ops {
                let why = if *why { "because" } else { "" };
                let input = match TOOLS[*tool] {
                    "merge" => json!({"ids": [a, b], "text": "merged text", "kind": KINDS[*kind], "why": why}),
                    "split" => json!({"id": a, "into": [{"text": "part one"}, {"text": "part two", "kind": KINDS[*kind]}], "why": why}),
                    "change_type" => json!({"id": a, "kind": KINDS[*kind], "why": why}),
                    "supersede" => json!({"id": a, "text": "newer text", "why": why}),
                    _ => json!({"id": a, "why": why}),
                };
                phase.call(&format!("sleep_memory_{}", TOOLS[*tool]), input).await;
            }
            Some(phase.finish(true).await.expect("finish"))
        });
        let Some(summary) = summary else {
            return Ok(());
        };

        let keep_recent = i64::try_from(h.kernel.shared().cfg().sleep.keep_recent.as_nanos()).expect("fits");
        let counts: (i64, i64, i64, i64) = rt.block_on(h.query(move |c| {
            Ok(c.query_row(
                "SELECT (SELECT COUNT(*) FROM memory WHERE kind = 'core' AND valid_to IS NOT NULL),
                        (SELECT COUNT(*) FROM memory WHERE valid_to IS NOT NULL AND created_at > ?1),
                        (SELECT COUNT(*) FROM memory_archive WHERE reason = 'forget'),
                        (SELECT COUNT(*) FROM memory_archive WHERE run_id IS NULL)",
                [t - keep_recent],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?)
        }));
        prop_assert_eq!(counts.0, 0, "core never closes");
        prop_assert_eq!(counts.1, 0, "nothing younger than keep_recent closes");
        prop_assert!(counts.2 <= 2, "forgets within max_forget");
        prop_assert_eq!(counts.3, 0, "every archive row carries the night");
        prop_assert!(summary.applied <= 6, "applied within max_ops");

        let rank = |v: &str| VIS.iter().position(|x| *x == v).expect("visibility");
        let after: Vec<(i64, String)> = rt.block_on(h.query(|c| {
            let mut stmt = c.prepare("SELECT id, visibility FROM memory ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            Ok(rows)
        }));
        for (id, vis) in &before {
            let now = after.iter().find(|(a, _)| a == id).map(|(_, v)| v.as_str()).expect("rows are never deleted");
            prop_assert!(rank(now) >= rank(vis), "visibility of {} never loosens", id);
        }
    });
}

/// Промпт фазы 1 байт-стабилен (`08 §5`, I13): английский, из снапшота.
#[tokio::test]
async fn the_memory_prompt_is_byte_stable() {
    let h = start_sleepy(|_| {}).await;
    seed_corpus(&h).await;
    let shared = h.kernel.shared();
    let phase = memory::Phase::build(shared, RunId::from("night"), shared.now())
        .await
        .expect("build")
        .expect("corpus");
    insta::assert_snapshot!(phase.prompt());
}
