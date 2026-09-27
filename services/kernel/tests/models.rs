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

/// Встроенный провайдер, стартовавший без каталога моделей, дочитывает его
/// фоном: модели каталога появляются в реестре без рестарта ядра.
#[tokio::test]
async fn a_late_listing_reaches_the_running_registry() {
    use std::time::Duration;

    use eva_protocol::types::ModelId;
    use eva_providers::mock::MockProvider;

    let provider = MockProvider::new(common::mock_config()).awaiting_listing();
    let h = common::start_with_provider(provider.clone()).await;
    let names = async || -> Vec<String> {
        h.client_session()
            .await
            .models()
            .list()
            .call(list::Args {})
            .await
            .expect("list")
            .models
            .into_iter()
            .map(|m| m.decl.name.as_str().to_owned())
            .collect()
    };
    assert_eq!(names().await, [common::MODEL], "config models only");

    let late = eva_protocol::types::model::ModelDecl {
        name: ModelId::from("late-model"),
        ..common::model_decl()
    };
    provider.list(vec![late]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let names = names().await;
        if names.iter().any(|n| n == "late-model") {
            assert_eq!(
                names,
                ["late-model", common::MODEL],
                "by name, config models kept"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the listing never reached the registry: {names:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
