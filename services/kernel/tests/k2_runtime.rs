//! Компонент `modules` и счётчики за пределами handshake: сутки `system.tz`
//! (`09 §1`), проверки конфига на старте, `offline → forgotten` без
//! рестарта (`06 §8`), `modules.forbidden` (`09 §5`).

mod common;

use std::{collections::BTreeMap, time::Duration};

use common::{FaultyDb, Harness, OTHER, SVC, service};
use eva::{
    time::{Clock as _, Timestamp},
    utils::SecretString,
};
use eva_core::Caller;
use eva_core::{CallStep as _, service::surfaces::Surfaces as _};
use eva_kernel::{
    Config, Kernel, Parts,
    config::chats::MasterIdentity,
    db::{Db as _, Queries as _, stats::Metric},
    subsystems::modules,
    world::testing,
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::surfaces::get,
    types::{ModuleId, SurfaceId},
};

/// Последняя секунда 2026-03-01 в зоне с этим смещением от UTC.
fn last_second_of_march_1(offset_secs: i64) -> u64 {
    const MARCH_2_UTC: i64 = 1_772_409_600;
    u64::try_from(MARCH_2_UTC - offset_secs - 1).expect("positive")
}

async fn days(h: &Harness, metric: &'static str) -> Vec<(String, i64)> {
    h.query(move |c| {
        let mut stmt =
            c.prepare("SELECT day, count FROM health_stats WHERE metric = ?1 ORDER BY day")?;
        let rows = stmt
            .query_map([metric], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

#[tokio::test]
async fn counters_split_days_at_midnight_of_system_tz() {
    for (tz, offset) in [
        ("UTC", 0),
        ("+03:00", 3 * 3600),
        ("-05:30", -(5 * 3600 + 1800)),
    ] {
        let h = common::start_with(|cfg| cfg.system.tz = Some(String::from(tz))).await;
        let mut clock = h.clock.clone();
        clock.set(Timestamp::from_secs(last_second_of_march_1(offset)));
        h.kernel.shared().bump(Metric::ModulesForbidden, SVC).await;
        clock.advance(Duration::from_secs(1));
        h.kernel.shared().bump(Metric::ModulesForbidden, SVC).await;

        let expected = [
            (String::from("2026-03-01"), 1),
            (String::from("2026-03-02"), 1),
        ];
        assert_eq!(days(&h, "modules.forbidden").await, expected, "{tz}");
    }
}

/// Одна зона даёт разные смещения зимой и летом: фиксированное смещение так не умеет.
#[tokio::test]
async fn an_iana_zone_cuts_days_with_dst() {
    let h = common::start_with(|cfg| cfg.system.tz = Some(String::from("America/New_York"))).await;
    let mut clock = h.clock.clone();
    // 04:30Z: летом (EDT, −4) уже 00:30 следующих суток, зимой (EST, −5) ещё 23:30.
    for utc in [1_782_880_200, 1_767_241_800] {
        clock.set(Timestamp::from_secs(utc));
        h.kernel.shared().bump(Metric::ModulesForbidden, SVC).await;
    }

    let expected = [
        (String::from("2025-12-31"), 1),
        (String::from("2026-07-01"), 1),
    ];
    assert_eq!(days(&h, "modules.forbidden").await, expected);
}

async fn start(tweak: impl FnOnce(&mut Config)) -> eyre::Result<Kernel<common::Test>> {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cfg = common::config(dir.path());
    tweak(&mut cfg);
    Kernel::start(
        cfg.clone(),
        Parts {
            demiurge: testing::Demiurge::new(7),
            db: FaultyDb::open(&cfg.db_path()).await,
            providers: BTreeMap::new(),
            hands: BTreeMap::new(),
        },
    )
    .await
}

#[tokio::test]
async fn start_refuses_what_it_cannot_honour() {
    let err = start(|cfg| cfg.system.tz = Some(String::from("Mars/Olympus")))
        .await
        .err()
        .expect("an unknown zone cannot cut days");
    assert!(err.to_string().contains("Mars/Olympus"), "{err}");

    let err = start(|cfg| {
        common::with_module_tokens(cfg);
        if let Some(entry) = cfg.modules.tokens.get_mut(&ModuleId::from(SVC)) {
            entry.token = SecretString::Plaintext(String::new());
        }
    })
    .await
    .err()
    .expect("an empty config token lets anyone in");
    assert!(err.to_string().contains("modules.tokens.svc"), "{err}");

    let err = start(|cfg| {
        cfg.chats.persons.master = vec![MasterIdentity {
            surface: SurfaceId::from("local"),
            external_id: String::from("me"),
            module: Some(ModuleId::from(SVC)),
        }];
    })
    .await
    .err()
    .expect("no module creates a builtin surface");
    assert!(err.to_string().contains("chats.persons.master"), "{err}");

    for tz in ["+05:45", "Europe/Moscow"] {
        assert!(
            start(|cfg| cfg.system.tz = Some(String::from(tz)))
                .await
                .is_ok(),
            "{tz}"
        );
    }
}

async fn state(h: &Harness, module: &'static str) -> String {
    h.query(move |c| {
        Ok(
            c.query_row("SELECT state FROM modules WHERE name = ?1", [module], |r| {
                r.get(0)
            })?,
        )
    })
    .await
}

async fn went_offline(h: &Harness, module: &'static str) {
    h.register_module(module).await;
    let now = h.clock.get();
    h.kernel
        .shared()
        .db()
        .write(move |tx| tx.modules().disconnected(&ModuleId::from(module), now))
        .await
        .expect("offline");
}

#[tokio::test]
async fn offline_modules_are_forgotten_while_the_kernel_runs() {
    let h = common::start().await;
    went_offline(&h, "gone").await;
    let mut clock = h.clock.clone();
    clock.advance(Duration::from_secs(8 * 24 * 60 * 60));
    went_offline(&h, "recent").await;

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let ticker = tokio::spawn(modules::forget_every(
        h.kernel.shared().clone(),
        Duration::from_millis(20),
        async {
            _ = stopped.await;
        },
    ));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while state(&h, "gone").await != "forgotten" {
        assert!(tokio::time::Instant::now() < deadline, "never forgotten");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state(&h, "recent").await, "offline");

    _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), ticker)
        .await
        .expect("the ticker stops")
        .expect("the ticker does not panic");
}

#[tokio::test]
async fn forbidden_calls_are_counted_per_module() {
    let h = common::start().await;
    let shared = h.kernel.shared();
    modules::forbidden(shared, &service(SVC)).await;
    modules::forbidden(shared, &service(SVC)).await;
    modules::forbidden(shared, &Caller::Kernel).await;

    let rows = h
        .query(|c| {
            let mut stmt = c.prepare(
                "SELECT key, count FROM health_stats WHERE metric = 'modules.forbidden'",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await;
    assert_eq!(rows, [(String::from(SVC), 2)]);
}

#[tokio::test]
async fn a_policy_refusal_bumps_modules_forbidden() {
    let h = common::start().await;
    h.service_with_surface(SVC).await;
    h.register_module(OTHER).await;
    let mut other = h.session(service(OTHER)).await;

    let res = other
        .surfaces()
        .get()
        .call(get::Args {
            surface: SurfaceId::from(SVC),
        })
        .await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Forbidden(_)))),
        "{res:?}"
    );

    let rows = h
        .query(|c| {
            let mut stmt = c.prepare(
                "SELECT key, count FROM health_stats WHERE metric = 'modules.forbidden'",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await;
    assert_eq!(rows, [(String::from(OTHER), 1)]);
}
