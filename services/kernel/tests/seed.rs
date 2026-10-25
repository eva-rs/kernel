//! Сид первого старта (`10 §2.10`, `05 §1.5`).

mod common;

use eva::handling::CallStep as _;
use eva_kernel::{
    config::chats::MasterIdentity,
    db::{DbResult, Queries as _},
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::Surfaces::{Trait as _, create},
    types::{ModuleId, PersonId, SurfaceId, surface::Builtin},
};

fn master(surface: &str, external_id: &str) -> MasterIdentity {
    MasterIdentity {
        surface: SurfaceId::from(surface),
        external_id: external_id.to_owned(),
        module: None,
    }
}

fn bound(surface: &str, external_id: &str, module: &str) -> MasterIdentity {
    MasterIdentity {
        module: Some(ModuleId::from(module)),
        ..master(surface, external_id)
    }
}

async fn folders(h: &common::Harness, surface: &'static str) -> Vec<String> {
    h.query(move |c| {
        let mut stmt = c.prepare(
            "SELECT name FROM folders WHERE surface_id = ?1 AND builtin = 1 ORDER BY name",
        )?;
        let names = stmt
            .query_map([surface], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(names)
    })
    .await
}

/// `(id, name, master, whitelisted)` всех личностей.
async fn persons(h: &common::Harness) -> Vec<(String, String, bool, bool)> {
    h.query(|c| -> DbResult<_> {
        let mut stmt =
            c.prepare("SELECT id, name, master, whitelisted FROM persons ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

#[tokio::test]
async fn builtin_surfaces_folders_and_system_person() {
    let h = common::start().await;

    for (id, builtin) in [("local", Builtin::Local), ("system", Builtin::System)] {
        let row = h
            .query(move |c| c.surfaces().get(&SurfaceId::from(id)))
            .await
            .unwrap_or_else(|| panic!("surface {id} is seeded"));
        assert_eq!(row.builtin, Some(builtin));
        assert_eq!(row.trusted, Some(true));
        assert_eq!(row.module, None);
    }

    assert_eq!(folders(&h, "local").await, ["Unsorted", "agents"]);
    assert_eq!(
        folders(&h, "system").await,
        ["Unsorted", "agents", "cron", "notify", "sleep"]
    );

    assert_eq!(
        persons(&h).await,
        [(String::from("p0"), String::from("system"), true, true)]
    );
}

#[tokio::test]
async fn master_from_config_is_seeded_once() {
    let h = common::start_with(|cfg| {
        cfg.chats.persons.master = vec![master("local", "owner")];
        cfg.chats.persons.master_name = String::from("Nero");
    })
    .await;

    let expected = [
        (String::from("p0"), String::from("system"), true, true),
        (String::from("p1"), String::from("Nero"), true, true),
    ];
    assert_eq!(persons(&h).await, expected);
    let owner = h
        .query(|c| {
            c.persons()
                .identity_owner(&SurfaceId::from("local"), "owner")
        })
        .await;
    assert_eq!(owner, Some(PersonId::from("p1")));

    // Рестарт на той же базе ничего не множит.
    let common::Harness { kernel, dir, .. } = h;
    drop(kernel);
    let mut cfg = common::config(dir.path());
    cfg.chats.persons.master = vec![master("local", "owner")];
    let h = common::start_in(dir, cfg).await;
    assert_eq!(persons(&h).await, expected);
}

#[tokio::test]
async fn master_on_a_later_surface_joins_the_same_person() {
    let h = common::start_with(|cfg| {
        cfg.chats.persons.master = vec![master("local", "owner"), master("svc", "42")];
    })
    .await;
    assert_eq!(persons(&h).await.len(), 2, "p0 and the master");

    h.service_with_surface(common::SVC).await;

    let owner = h
        .query(|c| c.persons().identity_owner(&SurfaceId::from("svc"), "42"))
        .await;
    assert_eq!(owner, Some(PersonId::from("p1")));
    assert_eq!(persons(&h).await.len(), 2, "no second master person");
}

#[tokio::test]
async fn master_is_removed_only_by_config() {
    let h = common::start_with(|cfg| {
        cfg.chats.persons.master = vec![master("local", "owner")];
    })
    .await;

    let common::Harness { kernel, dir, .. } = h;
    drop(kernel);
    let cfg = common::config(dir.path());
    let h = common::start_in(dir, cfg).await;

    let rows = persons(&h).await;
    assert_eq!(
        rows[1],
        (String::from("p1"), String::from("Master"), false, true)
    );
}

fn create_svc() -> create::Args {
    create::Args {
        id: SurfaceId::from(common::SVC),
        name: String::from("svc surface"),
        rules: None,
    }
}

/// Иначе любой сервис, первым создавший surface адреса хозяина, выдавал бы
/// себе master-туры.
#[tokio::test]
async fn a_bound_master_surface_is_created_only_by_its_module() {
    let h = common::start_with(|cfg| {
        cfg.chats.persons.master = vec![bound(common::SVC, "42", common::SVC)];
    })
    .await;

    h.register_module(common::OTHER).await;
    let mut other = h.session(common::service(common::OTHER)).await;
    let res = other
        .surfaces()
        .inner_mut()
        .create()
        .call(create_svc())
        .await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Forbidden(_)))),
        "{res:?}"
    );
    assert_eq!(
        persons(&h).await.len(),
        1,
        "no master seeded for a squatter"
    );

    h.service_with_surface(common::SVC).await;
    let owner = h
        .query(|c| c.persons().identity_owner(&SurfaceId::from("svc"), "42"))
        .await;
    assert_eq!(owner, Some(PersonId::from("p1")));
}

#[tokio::test]
async fn a_bound_pair_on_a_surface_of_another_module_is_not_seeded() {
    let h = common::start().await;
    h.service_with_surface(common::SVC).await;

    let common::Harness { kernel, dir, .. } = h;
    drop(kernel);
    let mut cfg = common::config(dir.path());
    cfg.chats.persons.master = vec![bound(common::SVC, "42", common::OTHER)];
    let h = common::start_in(dir, cfg).await;

    let owner = h
        .query(|c| c.persons().identity_owner(&SurfaceId::from("svc"), "42"))
        .await;
    assert_eq!(owner, None);
    assert_eq!(persons(&h).await.len(), 1, "only p0");
}
