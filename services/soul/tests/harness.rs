//! Плюмбинг soul под харнессом: мир, реестры, руки и диалоговое API.

mod common;

use std::{sync::atomic::Ordering, time::Duration};

use eva::handling::CallStep as _;
use eva_protocol::{
    errors::{Aux, Generic},
    requests::Tools::{self, Trait as _},
    types::{ChatId, ModelId, TurnId},
};

use eva_soul::world::Demiurge as _;

#[tokio::test]
async fn world_mints_uuid_v7_from_the_test_clock() {
    let mut h = common::start().await;
    h.clock.advance(Duration::from_secs(3600));

    let mut world = h.soul.shared().demiurge().make_world();
    let (a, b): (TurnId, TurnId) = (world.mint(), world.mint());
    assert_ne!(a, b);

    let uuid = uuid::Uuid::parse_str(a.as_str()).expect("uuid");
    assert_eq!(uuid.get_version_num(), 7);
    let (secs, _) = uuid.get_timestamp().expect("v7 timestamp").to_unix();
    assert_eq!(secs, world.now().as_secs());
}

#[tokio::test]
async fn registries_are_built_at_start() {
    let h = common::start().await;
    let shared = h.soul.shared();

    let models = shared.models();
    let entry = models
        .get(&ModelId::from(common::MODEL))
        .expect("mock model is registered");
    assert_eq!(entry.provider, "mock");
    assert!(shared.provider("mock").is_some());

    let endpoint = shared.hands().get(common::ENDPOINT).expect("endpoint");
    let limits = endpoint.limits().await.expect("limits from greeting");
    assert_eq!(limits.roots, [h.dir.path().join("work")]);
    assert_eq!(
        h.hands_calls.load(Ordering::Relaxed),
        1,
        "one greeting at start"
    );

    h.hands_unavailable.store(true, Ordering::Relaxed);
    endpoint.forget_limits().await;
    let err = endpoint.limits().await.expect_err("machine is off");
    assert!(err.is_unavailable());
}

#[tokio::test]
async fn turn_operations_validate_objects_and_catalog_is_registered() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let res = client.turns().cancel(ChatId(1)).await;
    assert!(matches!(&res, Err(Generic::Spec(_))), "{res:?}");

    let res = client.turns().attach(ChatId(1), None).await.map(|ok| ok.0);
    assert!(
        matches!(&res, Err(Generic::Spec(_))),
        "missing chat is rejected before creating a stream"
    );

    let res = client
        .tools()
        .inner_mut()
        .catalog()
        .call(Tools::catalog::Args::default())
        .await
        .expect("tool catalog is available");
    assert!(!res.entries.is_empty());

    let mut svc = h.session(common::service(common::SVC)).await;
    let res = svc
        .chats()
        .notify(String::from("hi"), String::from("body"), None)
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Forbidden(_)))),
        "notify is soul-only: {res:?}"
    );
}
