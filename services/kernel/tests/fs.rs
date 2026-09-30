//! `fs.*` (`06 §5`, I12): файлы модуля в `<state_dir>/modules/<module>/fs`;
//! `..`, абсолютный путь и симлинк наружу — `forbidden`; квоты байтов и
//! записей не обходятся параллельной записью; `read` оставляет место конверту кадра.

mod common;

use std::path::PathBuf;

use common::{
    Harness, OTHER, Outcome, SVC, TestSession, matrix_callers, outcome, service, with_modules,
};
use eva::bytesize::ByteSize;
use eva_core::{CallStep as _, service::fs::Fs as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::fs::{EntryKind, list, read, remove, stat, write},
    types::Bytes,
    wire::Capabilities,
};
use eva_rpc::NoHandler;

fn root(h: &Harness, module: &str) -> PathBuf {
    h.dir.path().join("modules").join(module).join("fs")
}

fn put(path: &str, data: &[u8]) -> write::Args {
    write::Args {
        path: path.to_owned(),
        data: Bytes::from(data),
        create_dirs: false,
    }
}

async fn write_ok(session: &mut TestSession, path: &str, data: &[u8], create_dirs: bool) {
    session
        .fs()
        .write()
        .call(write::Args {
            create_dirs,
            ..put(path, data)
        })
        .await
        .expect("fs.write");
}

async fn read(session: &mut TestSession, path: &str) -> Result<Vec<u8>, Generic<read::Err>> {
    session
        .fs()
        .read()
        .call(read::Args {
            path: path.to_owned(),
        })
        .await
        .map(|ok| ok.data.into_vec())
}

async fn names(session: &mut TestSession, path: &str) -> Vec<(String, EntryKind, u64)> {
    session
        .fs()
        .list()
        .call(list::Args {
            path: path.to_owned(),
        })
        .await
        .expect("fs.list")
        .entries
        .into_iter()
        .map(|e| (e.name, e.kind, e.size))
        .collect()
}

#[tokio::test]
async fn write_read_list_stat_remove() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    let write::Ok { size } = svc
        .fs()
        .write()
        .call(put("notes.txt", b"hello"))
        .await
        .expect("write");
    assert_eq!(size, 5);
    assert_eq!(
        std::fs::read(root(&h, SVC).join("notes.txt")).expect("on disk"),
        b"hello"
    );
    assert_eq!(read(&mut svc, "notes.txt").await.expect("read"), b"hello");
    assert_eq!(
        read(&mut svc, "./notes.txt").await.expect("read ./"),
        b"hello"
    );

    write_ok(&mut svc, "a/b/c.bin", &[0, 1, 2], true).await;
    assert_eq!(
        names(&mut svc, "").await,
        [
            (String::from("a"), EntryKind::Dir, 0),
            (String::from("notes.txt"), EntryKind::File, 5),
        ]
    );
    assert_eq!(
        names(&mut svc, "a/b").await,
        [(String::from("c.bin"), EntryKind::File, 3)]
    );

    let stat::Ok { entry } = svc
        .fs()
        .stat()
        .call(stat::Args {
            path: String::from("a/b/c.bin"),
        })
        .await
        .expect("stat");
    assert_eq!(
        (entry.name.as_str(), entry.kind, entry.size),
        ("c.bin", EntryKind::File, 3)
    );
    let stat::Ok { entry } = svc
        .fs()
        .stat()
        .call(stat::Args {
            path: String::new(),
        })
        .await
        .expect("stat root");
    assert_eq!(entry.kind, EntryKind::Dir);

    svc.fs()
        .remove()
        .call(remove::Args {
            path: String::from("notes.txt"),
            recursive: false,
        })
        .await
        .expect("remove file");
    let res = svc
        .fs()
        .remove()
        .call(remove::Args {
            path: String::from("a"),
            recursive: false,
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "non-empty dir: {res:?}");
    svc.fs()
        .remove()
        .call(remove::Args {
            path: String::from("a"),
            recursive: true,
        })
        .await
        .expect("remove dir");
    assert!(names(&mut svc, "").await.is_empty());
}

#[tokio::test]
async fn spec_errors() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    write_ok(&mut svc, "dir/file", b"x", true).await;

    let res = read(&mut svc, "missing").await;
    assert!(
        matches!(&res, Err(Generic::Spec(read::Err::NotFound(e))) if e.path == "missing"),
        "{res:?}"
    );
    let res = read(&mut svc, "dir").await;
    assert!(
        matches!(&res, Err(Generic::Spec(read::Err::IsDir(e))) if e.path == "dir"),
        "{res:?}"
    );
    let res = read(&mut svc, "dir/file/deeper").await;
    assert!(
        matches!(&res, Err(Generic::Spec(read::Err::NotFound(_)))),
        "{res:?}"
    );

    let res = svc.fs().write().call(put("new/file", b"x")).await;
    assert!(
        matches!(&res, Err(Generic::Spec(write::Err::NotFound(e))) if e.path == "new/file"),
        "missing parent without create_dirs: {res:?}"
    );
    let res = svc.fs().write().call(put("dir", b"x")).await;
    assert!(
        matches!(&res, Err(Generic::Spec(write::Err::IsDir(_)))),
        "{res:?}"
    );
    let res = svc.fs().write().call(put("", b"x")).await;
    assert!(
        matches!(&res, Err(Generic::Spec(write::Err::IsDir(_)))),
        "root: {res:?}"
    );
    let res = svc.fs().write().call(put("dir/file/x", b"x")).await;
    assert_eq!(outcome(&res), Outcome::Invalid, "parent is a file: {res:?}");

    let res = svc
        .fs()
        .list()
        .call(list::Args {
            path: String::from("nope"),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(list::Err::NotFound(_)))),
        "{res:?}"
    );
    let res = svc
        .fs()
        .list()
        .call(list::Args {
            path: String::from("dir/file"),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(list::Err::NotDir(e))) if e.path == "dir/file"),
        "{res:?}"
    );

    let res = svc
        .fs()
        .stat()
        .call(stat::Args {
            path: String::from("nope"),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(stat::Err::NotFound(_)))),
        "{res:?}"
    );
    let res = svc
        .fs()
        .remove()
        .call(remove::Args {
            path: String::from("nope"),
            recursive: true,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(remove::Err::NotFound(_)))),
        "{res:?}"
    );
    let res = svc
        .fs()
        .remove()
        .call(remove::Args {
            path: String::from("."),
            recursive: true,
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "the root stays: {res:?}");
}

#[tokio::test]
async fn escapes_are_forbidden() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    write_ok(&mut svc, "inside/file", b"x", true).await;
    std::fs::write(h.dir.path().join("secret"), b"kernel").expect("secret");

    for path in [
        "../secret",
        "inside/../../secret",
        "inside/../../../secret",
        "/etc/passwd",
        "..",
    ] {
        let res = read(&mut svc, path).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "read {path}: {res:?}");
        let res = svc.fs().write().call(put(path, b"pwn")).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "write {path}: {res:?}");
        let res = svc
            .fs()
            .list()
            .call(list::Args {
                path: path.to_owned(),
            })
            .await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "list {path}: {res:?}");
        let res = svc
            .fs()
            .stat()
            .call(stat::Args {
                path: path.to_owned(),
            })
            .await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "stat {path}: {res:?}");
        let res = svc
            .fs()
            .remove()
            .call(remove::Args {
                path: path.to_owned(),
                recursive: true,
            })
            .await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "remove {path}: {res:?}");
    }
    assert_eq!(
        std::fs::read(h.dir.path().join("secret")).expect("secret intact"),
        b"kernel"
    );
}

#[tokio::test]
async fn symlinks_outside_are_forbidden() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    write_ok(&mut svc, "real/file", b"inside", true).await;
    let outside = h.dir.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::fs::write(outside.join("secret"), b"kernel").expect("secret");

    let fs_root = root(&h, SVC);
    std::os::unix::fs::symlink(&outside, fs_root.join("out")).expect("link out");
    std::os::unix::fs::symlink(outside.join("secret"), fs_root.join("out_file")).expect("link");
    std::os::unix::fs::symlink(fs_root.join("real"), fs_root.join("alias")).expect("link in");

    for path in ["out/secret", "out_file"] {
        let res = read(&mut svc, path).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "read {path}: {res:?}");
        let res = svc.fs().write().call(put(path, b"pwn")).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "write {path}: {res:?}");
    }
    let res = svc
        .fs()
        .write()
        .call(write::Args {
            create_dirs: true,
            ..put("out/new/file", b"pwn")
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "{res:?}");
    let res = svc
        .fs()
        .list()
        .call(list::Args {
            path: String::from("out"),
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "{res:?}");

    assert_eq!(
        read(&mut svc, "alias/file").await.expect("link inside"),
        b"inside"
    );
    let listed: Vec<String> = names(&mut svc, "").await.into_iter().map(|e| e.0).collect();
    assert_eq!(listed, ["alias", "real"], "links outside are not listed");

    svc.fs()
        .remove()
        .call(remove::Args {
            path: String::from("out"),
            recursive: true,
        })
        .await
        .expect("removing the link itself");
    assert_eq!(
        std::fs::read(outside.join("secret")).expect("target intact"),
        b"kernel"
    );
    assert!(std::fs::symlink_metadata(fs_root.join("out")).is_err());
}

#[tokio::test]
async fn quota_counts_the_whole_directory() {
    let h = common::start_with(|cfg| cfg.modules.fs_quota = eva::bytesize::ByteSize::b(100)).await;
    let mut svc = h.session(service(SVC)).await;

    write_ok(&mut svc, "a", &[0; 60], false).await;
    let res = svc.fs().write().call(put("b", &[0; 60])).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
    write_ok(&mut svc, "a", &[0; 90], false).await;
    write_ok(&mut svc, "b", &[0; 10], false).await;
    let res = svc.fs().write().call(put("c", &[0; 1])).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
}

#[tokio::test]
async fn modules_do_not_see_each_other() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    let mut other = h.session(service(OTHER)).await;

    write_ok(&mut svc, "mine", b"svc", false).await;
    let res = read(&mut other, "mine").await;
    assert!(
        matches!(res, Err(Generic::Spec(read::Err::NotFound(_)))),
        "{res:?}"
    );
    let res = read(&mut other, &format!("../../{SVC}/fs/mine")).await;
    assert_eq!(outcome(&res), Outcome::Forbidden);
}

#[tokio::test]
async fn policy_matrix() {
    let h = common::start().await;

    for (who, caller) in matrix_callers() {
        let expect = if caller.is_kernel() {
            Outcome::Forbidden
        } else {
            Outcome::Ok
        };
        let mut session = h.session(caller).await;
        let res = session.fs().write().call(put("f", b"x")).await;
        assert_eq!(outcome(&res), expect, "{who} write");
        let res = read(&mut session, "f").await;
        assert_eq!(outcome(&res), expect, "{who} read");
        let res = session
            .fs()
            .stat()
            .call(stat::Args {
                path: String::from("f"),
            })
            .await;
        assert_eq!(outcome(&res), expect, "{who} stat");
        let res = session.fs().list().call(list::Args::default()).await;
        assert_eq!(outcome(&res), expect, "{who} list");
        let res = session
            .fs()
            .remove()
            .call(remove::Args {
                path: String::from("f"),
                recursive: false,
            })
            .await;
        assert_eq!(outcome(&res), expect, "{who} remove");
    }
}

#[tokio::test]
async fn dangling_symlinks_are_forbidden() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    write_ok(&mut svc, "inside", b"x", false).await;
    let outside = h.dir.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::os::unix::fs::symlink(outside.join("new"), root(&h, SVC).join("dangling"))
        .expect("dangling link");

    let res = svc.fs().write().call(put("dangling", b"pwn")).await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "write: {res:?}");
    let res = svc
        .fs()
        .write()
        .call(write::Args {
            create_dirs: true,
            ..put("dangling/x", b"pwn")
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "write under: {res:?}");
    let res = read(&mut svc, "dangling").await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "read: {res:?}");
    let res = svc
        .fs()
        .stat()
        .call(stat::Args {
            path: String::from("dangling"),
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "stat: {res:?}");
    assert!(!outside.join("new").exists(), "nothing was created outside");
}

#[tokio::test]
async fn concurrent_writes_share_one_quota() {
    let h = common::start_with(|cfg| cfg.modules.fs_quota = ByteSize::b(100)).await;

    let mut sessions = Vec::new();
    for _ in 0..16 {
        sessions.push(h.session(service(SVC)).await);
    }
    let writes = sessions
        .iter_mut()
        .enumerate()
        .map(|(i, session)| async move {
            session
                .fs()
                .write()
                .call(put(&format!("f{i}"), &[0; 60]))
                .await
        });
    let results = futures::future::join_all(writes).await;

    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "{results:?}");
    assert!(
        results
            .iter()
            .all(|r| r.is_ok() || matches!(r, Err(Generic::Aux(Aux::Quota(_))))),
        "{results:?}"
    );
    let used: u64 = std::fs::read_dir(root(&h, SVC))
        .expect("root")
        .map(|e| e.expect("entry").metadata().expect("meta").len())
        .sum();
    assert!(used <= 100, "{used}");
}

#[tokio::test]
async fn entry_count_is_capped() {
    let h = common::start_with(|cfg| cfg.modules.fs_max_entries = 3).await;
    let mut svc = h.session(service(SVC)).await;

    write_ok(&mut svc, "a", b"", false).await;
    write_ok(&mut svc, "b", b"", false).await;
    let res = svc
        .fs()
        .write()
        .call(write::Args {
            create_dirs: true,
            ..put("d/e/f", b"")
        })
        .await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Quota(_)))),
        "two dirs and a file do not fit: {res:?}"
    );
    write_ok(&mut svc, "c", b"", false).await;
    let res = svc.fs().write().call(put("d", b"")).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
    write_ok(&mut svc, "a", b"rewrite", false).await;
}

#[tokio::test]
async fn read_leaves_room_for_the_frame() {
    let h = common::start_with(|cfg| {
        with_modules(cfg);
        cfg.modules.transport.max_frame = ByteSize::kib(4);
    })
    .await;
    let listener = h.bind_service(SVC).await;
    let (_listening, _url) = h.listen().await;
    let mut svc = h.session(service(SVC)).await;
    let max = usize::try_from(h.kernel.shared().cfg().modules.max_payload()).expect("usize");
    write_ok(&mut svc, "near", &vec![7; 4000], false).await;
    write_ok(&mut svc, "fits", &vec![7; max], false).await;

    let (client, _) = common::dialed(&listener, Capabilities::default(), NoHandler).await;
    let mut remote = client.session();

    let res = remote
        .fs()
        .read()
        .call(read::Args {
            path: String::from("near"),
        })
        .await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Quota(_)))),
        "a file just under max_frame is quota, not a broken frame: {res:?}"
    );
    let ok = remote
        .fs()
        .read()
        .call(read::Args {
            path: String::from("fits"),
        })
        .await
        .expect("the largest readable file fits a frame");
    assert_eq!(ok.data.len(), max);
}
