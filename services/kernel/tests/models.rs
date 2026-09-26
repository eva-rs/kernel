//! `models.list` (`01 §8.2`, `11 §3`): реестр — только хозяину.

mod common;

use common::{OTHER, Outcome, SVC, outcome};
use eva_core::{CallStep as _, service::models::Models as _};
use eva_protocol::requests::models::list;

use Outcome::{Forbidden as F, Ok as K};

#[tokio::test]
async fn lists_the_registry() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let list::Ok { models } = client
        .models()
        .list()
        .call(list::Args {})
        .await
        .expect("list");
    assert_eq!(models.len(), 1);
    let entry = &models[0];
    assert_eq!(entry.decl.name.as_str(), common::MODEL);
    assert_eq!(entry.provider, "mock");
    assert_eq!(entry.decl.context_window, 200_000);
    assert_eq!(entry.decl.max_output, 8_192);
}

#[tokio::test]
async fn owner_only() {
    let h = common::start().await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    let want = [K, K, F, F];
    for ((who, caller), want) in common::matrix_callers().into_iter().zip(want) {
        let mut session = h.session(caller).await;
        let got = outcome(&session.models().list().call(list::Args {}).await);
        assert_eq!(got, want, "models.list as {who}");
    }
}
