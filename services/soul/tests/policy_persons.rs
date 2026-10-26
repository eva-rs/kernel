//! Матрица `Policy` (`11 §8`) для `persons.*`: `resolve` — по своему
//! surface, прочее — только хозяину.

mod common;

use common::{OTHER, Outcome, SVC, TestSession, outcome};
use eva::handling::CallStep as _;
use eva_protocol::{
    requests::Persons::{Trait as _, list, merge, rename, resolve, whitelist},
    types::{PersonId, SurfaceId},
};

use Outcome::{Forbidden as F, Ok as K};

#[derive(Debug, Clone, Copy)]
enum Case {
    ResolveA,
    ResolveLocal,
    ResolveSystem,
    List,
    Rename,
    Merge,
    Whitelist,
}

/// Ожидание по caller'ам: soul, client, service-own (A), service-foreign (B).
const MATRIX: &[(Case, [Outcome; 4])] = &[
    (Case::ResolveA, [K, K, K, F]),
    (Case::ResolveLocal, [K, K, F, F]),
    (Case::ResolveSystem, [K, K, F, F]),
    (Case::List, [K, K, F, F]),
    (Case::Rename, [K, K, F, F]),
    (Case::Merge, [K, K, F, F]),
    (Case::Whitelist, [K, K, F, F]),
];

fn resolve_args(surface: &str, external_id: &str) -> resolve::Args {
    resolve::Args {
        surface: SurfaceId::from(surface),
        external_id: String::from(external_id),
        handle: None,
        name: None,
    }
}

async fn person(owner: &mut TestSession, external_id: &str) -> PersonId {
    owner
        .persons()
        .inner_mut()
        .resolve()
        .call(resolve_args(SVC, external_id))
        .await
        .expect("target person")
        .person
        .id
}

async fn run(session: &mut TestSession, owner: &mut TestSession, case: Case, n: usize) -> Outcome {
    let target = person(owner, &format!("t{n}")).await;
    let into = person(owner, &format!("into{n}")).await;
    let mut persons_client = session.persons();
    let persons = persons_client.inner_mut();
    match case {
        Case::ResolveA => outcome(&persons.resolve().call(resolve_args(SVC, "x")).await),
        Case::ResolveLocal => outcome(&persons.resolve().call(resolve_args("local", "x")).await),
        Case::ResolveSystem => outcome(&persons.resolve().call(resolve_args("system", "x")).await),
        Case::List => outcome(&persons.list().call(list::Args::default()).await),
        Case::Rename => outcome(
            &persons
                .rename()
                .call(rename::Args {
                    person: target,
                    name: String::from("Renamed"),
                })
                .await,
        ),
        Case::Merge => outcome(
            &persons
                .merge()
                .call(merge::Args {
                    from: target,
                    into,
                    confirm: false,
                })
                .await,
        ),
        Case::Whitelist => outcome(
            &persons
                .whitelist()
                .call(whitelist::Args {
                    person: target,
                    on: true,
                })
                .await,
        ),
    }
}

#[tokio::test]
async fn persons_matrix() {
    let h = common::start().await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    let mut owner = h.soul_session().await;

    let mut failures = Vec::new();
    let mut n = 0;
    for &(case, expected) in MATRIX {
        for ((who, caller), want) in common::matrix_callers().into_iter().zip(expected) {
            n += 1;
            let mut session = h.session(caller).await;
            let got = run(&mut session, &mut owner, case, n).await;
            if got != want {
                failures.push(format!("{case:?} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
