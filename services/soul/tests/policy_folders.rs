//! Матрица `Policy` (`11 §8`) для `folders.*`: операции только хозяину.

mod common;

use common::{OTHER, Outcome, SVC, TestSession, outcome};
use eva::handling::CallStep as _;
use eva_protocol::{
    requests::{
        Folders::{Trait as _, create, delete, list, update},
        Patch,
    },
    types::{FolderId, SurfaceId},
};

use Outcome::{Forbidden as F, Ok as K};

#[derive(Debug, Clone, Copy)]
enum Case {
    CreateA,
    UpdateA,
    DeleteA,
    List,
}

/// Ожидание по caller'ам: soul, client, service-own (A), service-foreign (B).
const MATRIX: &[(Case, [Outcome; 4])] = &[
    (Case::CreateA, [K, K, F, F]),
    (Case::UpdateA, [K, K, F, F]),
    (Case::DeleteA, [K, K, F, F]),
    (Case::List, [K, K, F, F]),
];

async fn run(session: &mut TestSession, case: Case, who: &str, folder: FolderId) -> Outcome {
    let mut folders_client = session.folders();
    let folders = folders_client.inner_mut();
    match case {
        Case::CreateA => outcome(
            &folders
                .create()
                .call(create::Args {
                    surface: SurfaceId::from(SVC),
                    name: format!("by {who}"),
                })
                .await,
        ),
        Case::UpdateA => outcome(
            &folders
                .update()
                .call(update::Args {
                    folder,
                    update: update::Update {
                        trusted: Patch::Change(Some(false)),
                        ..Default::default()
                    },
                })
                .await,
        ),
        Case::DeleteA => outcome(&folders.delete().call(delete::Args { folder }).await),
        Case::List => outcome(
            &folders
                .list()
                .call(list::Args {
                    surface: Some(SurfaceId::from(SVC)),
                    ..Default::default()
                })
                .await,
        ),
    }
}

#[tokio::test]
async fn folders_matrix() {
    let h = common::start().await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    let mut owner = h.soul_session().await;

    let mut failures = Vec::new();
    let mut n = 0;
    for &(case, expected) in MATRIX {
        for ((who, caller), want) in common::matrix_callers().into_iter().zip(expected) {
            n += 1;
            let create::Ok { folder } = owner
                .folders()
                .create(SurfaceId::from(SVC), format!("target {n}"))
                .await
                .expect("target folder");
            let mut session = h.session(caller).await;
            let got = run(&mut session, case, who, folder.id).await;
            if got != want {
                failures.push(format!("{case:?} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
