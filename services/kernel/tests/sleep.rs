//! `sleep.*` (`08 §2.8`): журнал ночей keyset'ом, фазы в порядке и с
//! `ops_json` целиком, матрица `Policy`.

mod common;

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, service::sleep::Sleep as _};
use eva_kernel::db::{Db as _, DbResult};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        RunsLimit,
        sleep::{phase_runs, runs},
    },
    types::{
        RunId,
        sleep::{Phase, PhaseOp, Verdict},
    },
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
        .runs()
        .call(runs::Args {
            limit: RunsLimit::new(limit).expect("limit in range"),
            after: after.map(RunId::from),
        })
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
        .phase_runs()
        .call(phase_runs::Args {
            run: RunId::from("n4"),
        })
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
        .phase_runs()
        .call(phase_runs::Args {
            run: RunId::from("n1"),
        })
        .await
        .expect("night without phases");
    assert!(phases.is_empty());

    let res = session
        .sleep()
        .phase_runs()
        .call(phase_runs::Args {
            run: RunId::from("nope"),
        })
        .await;
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
        .phase_runs()
        .call(phase_runs::Args {
            run: RunId::from("n4"),
        })
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
        let runs = outcome(&s.sleep().runs().call(runs::Args::default()).await);
        let phases = outcome(
            &s.sleep()
                .phase_runs()
                .call(phase_runs::Args {
                    run: RunId::from("n4"),
                })
                .await,
        );
        for (op, got) in [("runs", runs), ("phase_runs", phases)] {
            if got != want {
                failures.push(format!("sleep.{op} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
