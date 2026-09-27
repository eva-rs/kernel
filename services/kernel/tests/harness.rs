//! Плюмбинг ядра под харнессом: мир, реестры, руки, операции поздних этапов.

mod common;

use std::{sync::atomic::Ordering, time::Duration};

use eva_core::{
    CallStep as _,
    service::{chats::Chats as _, tools::Tools as _, turns::Turns as _},
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{chats, tools, turns},
    types::{ChatId, ModelId, TurnId},
};

use eva_kernel::world::Demiurge as _;

#[tokio::test]
async fn world_mints_uuid_v7_from_the_test_clock() {
    let mut h = common::start().await;
    h.clock.advance(Duration::from_secs(3600));

    let mut world = h.kernel.shared().demiurge().make_world();
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
    let shared = h.kernel.shared();

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
async fn later_milestones_answer_unavailable() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let res = client
        .turns()
        .cancel()
        .call(turns::cancel::Args { chat: ChatId(1) })
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Unavailable(m))) if m.contains("M2_PENDING")),
        "{res:?}"
    );

    let res = client
        .turns()
        .attach()
        .call(turns::attach::Args {
            chat: ChatId(1),
            since_seq: None,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Unavailable(m))) if m.contains("M2_PENDING")),
        "stream refuses before the first item"
    );

    let res = client
        .chats()
        .search()
        .call(chats::search::Args {
            query: String::from("tea"),
            limit: Default::default(),
            since: None,
            surface: None,
            folder: None,
            include_deleted: false,
            view: None,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Unavailable(m))) if m.contains("M3_PENDING")),
        "{res:?}"
    );

    let res = client
        .tools()
        .catalog()
        .call(tools::catalog::Args::default())
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Unavailable(m))) if m.contains("M4_PENDING")),
        "{res:?}"
    );

    let mut svc = h.session(common::service(common::SVC)).await;
    let res = svc
        .chats()
        .notify()
        .call(chats::notify::Args {
            subject: String::from("hi"),
            body: String::from("body"),
            source_chat: None,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Forbidden(_)))),
        "notify is kernel-only before it exists: {res:?}"
    );
}
