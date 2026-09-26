//! Матрица `Policy` (`11 §8`) для `surfaces.*`: операция × caller'ы
//! `Kernel`, `Client`, `Service` своего surface A, `Service` чужого B, и
//! `Service` против builtin `local`/`system`. Остальные домены кладут свою
//! матрицу в `tests/policy_<ns>.rs` на тех же `common::{matrix_callers, outcome}`.

mod common;

use common::{OTHER, Outcome, SVC, TestSession, outcome};
use eva_core::{CallStep as _, service::surfaces::Surfaces as _};
use eva_protocol::{
    requests::{
        Patch,
        surfaces::{create, get, list, update},
    },
    types::SurfaceId,
};

use Outcome::{Forbidden as F, Ok as K};

#[derive(Debug, Clone, Copy)]
enum Case {
    GetA,
    GetLocal,
    GetSystem,
    List,
    UpdateNameA,
    UpdateTrustA,
    UpdateRulesLocal,
    UpdateNameSystem,
    CreateFresh,
    CreateSystem,
    CreateLocalBuiltin,
}

/// Ожидание по caller'ам: kernel, client, service-own (A), service-foreign (B).
const MATRIX: &[(Case, [Outcome; 4])] = &[
    (Case::GetA, [K, K, K, F]),
    (Case::GetLocal, [K, K, F, F]),
    (Case::GetSystem, [K, K, F, F]),
    (Case::List, [K, K, K, K]),
    (Case::UpdateNameA, [K, K, K, F]),
    (Case::UpdateTrustA, [K, K, F, F]),
    (Case::UpdateRulesLocal, [K, K, F, F]),
    (Case::UpdateNameSystem, [K, K, F, F]),
    (Case::CreateFresh, [F, K, K, K]),
    (Case::CreateSystem, [F, F, F, F]),
    (Case::CreateLocalBuiltin, [F, K, F, F]),
];

fn a() -> SurfaceId {
    SurfaceId::from(SVC)
}

fn rename(id: SurfaceId) -> update::Args {
    update::Args {
        surface: id,
        update: update::Update {
            name: Patch::Change(String::from("Renamed")),
            ..Default::default()
        },
    }
}

async fn run(session: &mut TestSession, case: Case, who: &str) -> Outcome {
    let mut surfaces = session.surfaces();
    match case {
        Case::GetA => outcome(&surfaces.get().call(get::Args { surface: a() }).await),
        Case::GetLocal => outcome(
            &surfaces
                .get()
                .call(get::Args {
                    surface: SurfaceId::from("local"),
                })
                .await,
        ),
        Case::GetSystem => outcome(
            &surfaces
                .get()
                .call(get::Args {
                    surface: SurfaceId::from("system"),
                })
                .await,
        ),
        Case::List => outcome(&surfaces.list().call(list::Args::default()).await),
        Case::UpdateNameA => outcome(&surfaces.update().call(rename(a())).await),
        Case::UpdateTrustA => outcome(
            &surfaces
                .update()
                .call(update::Args {
                    surface: a(),
                    update: update::Update {
                        trusted: Patch::Change(Some(false)),
                        ..Default::default()
                    },
                })
                .await,
        ),
        Case::UpdateRulesLocal => outcome(
            &surfaces
                .update()
                .call(update::Args {
                    surface: SurfaceId::from("local"),
                    update: update::Update {
                        rules: Patch::Change(Some(String::from("Local rules."))),
                        ..Default::default()
                    },
                })
                .await,
        ),
        Case::UpdateNameSystem => outcome(
            &surfaces
                .update()
                .call(rename(SurfaceId::from("system")))
                .await,
        ),
        Case::CreateFresh => outcome(
            &surfaces
                .create()
                .call(create::Args {
                    id: SurfaceId::from(format!("fresh_{}", who.replace('-', "_"))),
                    name: Some(String::from("Fresh")),
                    rules: None,
                    ask: false,
                    builtin: false,
                })
                .await,
        ),
        Case::CreateSystem => outcome(
            &surfaces
                .create()
                .call(create::Args {
                    id: SurfaceId::from("system"),
                    name: Some(String::from("Mine")),
                    rules: None,
                    ask: false,
                    builtin: false,
                })
                .await,
        ),
        Case::CreateLocalBuiltin => outcome(
            &surfaces
                .create()
                .call(create::Args {
                    id: SurfaceId::from("local"),
                    name: None,
                    rules: None,
                    ask: false,
                    builtin: true,
                })
                .await,
        ),
    }
}

#[tokio::test]
async fn surfaces_matrix() {
    let h = common::start().await;
    h.register_module(common::CLIENT).await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;

    let mut failures = Vec::new();
    for &(case, expected) in MATRIX {
        for ((who, caller), want) in common::matrix_callers().into_iter().zip(expected) {
            let mut session = h.session(caller).await;
            let got = run(&mut session, case, who).await;
            if got != want {
                failures.push(format!("{case:?} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn service_lists_only_own_surfaces() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;

    for (session, own) in [(&mut svc, SVC), (&mut other, OTHER)] {
        let list::Ok { found } = session
            .surfaces()
            .list()
            .call(list::Args::default())
            .await
            .expect("list");
        assert!(!found.is_empty());
        assert!(
            found
                .iter()
                .all(|s| s.module.as_ref().map(|m| m.as_str()) == Some(own)),
            "{own} sees {found:?}"
        );
    }
}
