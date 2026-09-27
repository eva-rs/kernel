//! Хендлеры `blobs.*` (`07 §6`, `06 §5`): байты в `work.blob_dir/<id>/<name>`,
//! строка — в `blobs`. `Service` видит только blob'ы чатов своих surface.
//!
//! Приём идёт во временный файл `work.blob_dir/.staging/<id>`: брошенный
//! приём (ошибка, отмена вызова) удаляется, принятый переносится rename'ом.
//! Живые blob'ы чатов одного владельца surface — до `work.blob_quota`.

use std::path::{Path, PathBuf};

use eva::time::Timestamp;
use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        ByteStream, Response,
        blobs::{get, put, stat},
    },
    types::{BlobId, Bytes},
};
use futures::StreamExt as _;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

use crate::{
    components::Components,
    convert,
    db::{
        Db as _, DbResult, Queries as _,
        blobs::{BLOB_FLOOR, BlobRow, Source},
    },
    handling::text,
    shared::Shared,
    state::{Policy, State},
};

/// Каталог недописанных приёмов внутри `blob_dir`: rename оттуда атомарен.
pub const STAGING: &str = ".staging";

/// Имя файла blob'а — leaf: без разделителей, `.`/`..` и управляющих символов.
fn check_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

/// Заявленный hash — 64 hex-символа blake3.
fn parse_hash(hash: &str) -> AuxResult<[u8; 32]> {
    let invalid = || Aux::Invalid(format!("hash {hash:?} is not a blake3 hex digest"));
    let bytes = hash.as_bytes();
    if bytes.len() != 64 {
        return Err(invalid());
    }
    let mut out = [0_u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        match (digit(pair[0]), digit(pair[1])) {
            (Some(hi), Some(lo)) => out[i] = (hi * 16 + lo) as u8,
            _ => return Err(invalid()),
        }
    }
    Ok(out)
}

fn io(what: &str, e: std::io::Error) -> Aux {
    Aux::internal(format_args!("blob store: {what}: {e}"))
}

fn file(dir: &Path, row: &BlobRow) -> PathBuf {
    dir.join(row.id.as_str()).join(&row.name)
}

/// Вес blob'а в `work.blob_quota`: не меньше `BLOB_FLOOR`.
fn charged(size: u64) -> u64 {
    size.max(BLOB_FLOOR)
}

fn over_quota(used: u64, quota: u64) -> Aux {
    Aux::Quota(format!(
        "blobs of this surface owner take {used} of work.blob_quota {quota} bytes"
    ))
}

/// Загрузка и `Policy` blob'а: `NotFound` — и истёкшему, `Service` — только свой чат.
async fn load<C: Components, E: From<errors::blobs::NotFound>>(
    state: &mut State<C>,
    id: BlobId,
) -> Response<BlobRow, E> {
    let State {
        shared,
        world,
        caller,
        db_conn,
        ..
    } = state;
    let policy = Policy::new(caller);

    let db = db_conn.acquire(shared).await?;
    let key = id.clone();
    let row = db
        .run(move |c| c.blobs().get(&key))
        .await?
        .filter(|row| row.expires_at > world.now())
        .ok_or_else(|| errors::blobs::NotFound { id }.spec())?;
    match &row.chat {
        Some(chat) => policy.chats().read(chat)?,
        None => policy.owner()?,
    }
    Ok(row)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Put<C: Components>(
    state: &mut State<C>,
    args: put::Args,
) -> Response<put::Ok, put::Err> {
    let State {
        shared,
        world,
        caller,
        db_conn,
        ..
    } = state;
    let put::Args {
        meta:
            put::Meta {
                chat,
                name,
                media_type,
                kind,
                size,
                hash,
            },
        mut body,
    } = args;
    let policy = Policy::new(caller);
    let now = world.now();

    let db = db_conn.acquire(shared).await?;
    let found = db
        .run(move |c| -> DbResult<_> {
            match c.chats().chat_ref(chat)? {
                Some(chat_ref) => {
                    let used = c
                        .blobs()
                        .live_bytes(chat_ref.surface.module.as_ref(), now)?;
                    Ok(Some((chat_ref, used)))
                }
                None => Ok(None),
            }
        })
        .await?;
    // Приём долгий: соединение пула ему не нужно.
    db_conn.release();
    let (chat_ref, used) = found.ok_or_else(|| Aux::NotFound(format!("chat {chat}")))?;
    policy.chats().read(&chat_ref)?;

    if !check_name(&name) {
        return Err(errors::blobs::BadName { name }.spec());
    }
    (!media_type.trim().is_empty() && !media_type.chars().any(char::is_control)).true_or(
        Aux::Invalid("media_type must be a non-empty single line".into()),
    )?;
    text::bytes("media_type", &media_type, text::MEDIA_TYPE_MAX)?;
    let work = &shared.cfg().work;
    let (max, quota) = (work.blob_max.as_u64(), work.blob_quota.as_u64());
    (size <= max).true_or(Aux::Quota(format!(
        "blob is {size} bytes, work.blob_max is {max}"
    )))?;
    (used.saturating_add(charged(size)) <= quota).true_or(over_quota(used, quota))?;
    let declared = hash.as_deref().map(parse_hash).transpose()?;

    let row = BlobRow {
        id: world.mint(),
        kind,
        chat: Some(chat_ref),
        name,
        media_type,
        size,
        hash: Vec::new(),
        source: Source::Upload,
        created_at: now,
        expires_at: expires(now, *work.blob_keep),
    };
    let staging = Staging::new(&shared.cfg().blob_dir(), &row.id);

    let received = receive(&staging.path, &mut body, size).await;
    let checked = received.and_then(|(actual, digest)| {
        if actual != size {
            return Err(errors::blobs::SizeMismatch {
                declared: size,
                actual,
            }
            .spec());
        }
        match declared {
            Some(declared) if declared != *digest.as_bytes() => Err(errors::blobs::HashMismatch {
                declared: convert::blob::hex(&declared),
                actual: digest.to_hex().to_string(),
            }
            .spec()),
            _ => Ok(digest),
        }
    });
    let digest = match checked {
        Ok(digest) => digest,
        Err(e) => {
            staging.discard().await;
            return Err(e);
        }
    };

    let row = BlobRow {
        hash: digest.as_bytes().to_vec(),
        ..row
    };
    let (id, log_row) = (row.id.clone(), row.id.clone());
    // Отдельная задача: отмена вызова после приёма не оставит файл без строки.
    tokio::spawn(commit(shared.clone(), staging, row, quota, now))
        .await
        .map_err(|e| Aux::internal(format_args!("blob commit task failed: {e}")))??;

    log::info!(blob:% = log_row, chat:% = chat, size, caller = caller.label(); "blob stored");
    Ok(put::Ok {
        id,
        size,
        hash: digest.to_hex().to_string(),
    })
}

/// Файл приёма в `blob_dir/.staging`; не перенесённый на место удаляется на drop.
struct Staging {
    path: PathBuf,
    armed: bool,
}

impl Staging {
    fn new(blob_dir: &Path, id: &BlobId) -> Self {
        Self {
            path: blob_dir.join(STAGING).join(id.as_str()),
            armed: true,
        }
    }

    /// Отвергнутый приём уходит до ответа, а не фоном.
    async fn discard(mut self) {
        self.armed = false;
        let path = std::mem::take(&mut self.path);
        if let Err(e) = tokio::task::spawn_blocking(move || remove_staged(&path)).await {
            log::warn!(error:% = e; "blob staging cleanup task failed");
        }
    }
}

fn remove_staged(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(path:? = path, error:% = e; "failed to remove an abandoned blob upload");
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let path = std::mem::take(&mut self.path);
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => drop(rt.spawn_blocking(move || remove_staged(&path))),
            Err(_) => remove_staged(&path),
        }
    }
}

/// Файл на место, затем строка под квотой в транзакции писателя: параллельные
/// приёмы одного владельца её не обойдут.
async fn commit<C: Components>(
    shared: Shared<C>,
    mut staging: Staging,
    row: BlobRow,
    quota: u64,
    now: Timestamp,
) -> Response<(), put::Err> {
    let dir = shared.cfg().blob_dir().join(row.id.as_str());
    let (from, to, target) = (staging.path.clone(), dir.join(&row.name), dir.clone());
    let moved = tokio::task::spawn_blocking(move || {
        std::fs::create_dir(&target)?;
        std::fs::rename(&from, &to)
    })
    .await
    .map_err(|e| Aux::internal(format_args!("blob store task failed: {e}")))?;
    if let Err(e) = moved {
        discard(&dir).await;
        staging.discard().await;
        return Err(io("store", e).into());
    }
    staging.armed = false;

    let owner = row.chat.as_ref().and_then(|c| c.surface.module.clone());
    let size = row.size;
    let inserted = shared
        .db()
        .write(move |tx| -> AuxResult<()> {
            let used = tx.blobs().live_bytes(owner.as_ref(), now)?;
            (used.saturating_add(charged(size)) <= quota).true_or(over_quota(used, quota))?;
            Ok(tx.blobs().insert(&row)?)
        })
        .await;
    if let Err(e) = inserted {
        discard(&dir).await;
        return Err(e.into());
    }
    Ok(())
}

/// Приём тела с проверкой по ходу: перебор заявленного размера рвёт приём.
async fn receive(
    path: &Path,
    body: &mut ByteStream,
    declared: u64,
) -> Response<(u64, blake3::Hash), put::Err> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| io("create dir", e))?;
    }
    let mut out = tokio::fs::File::create(path)
        .await
        .map_err(|e| io("create file", e))?;
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_u64;

    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        total = total.saturating_add(chunk.len() as u64);
        if total > declared {
            return Err(errors::blobs::SizeMismatch {
                declared,
                actual: total,
            }
            .spec());
        }
        hasher.update(&chunk);
        out.write_all(&chunk).await.map_err(|e| io("write", e))?;
    }
    out.sync_all().await.map_err(|e| io("sync", e))?;
    Ok((total, hasher.finalize()))
}

/// Каталог отвергнутого blob'а уносится целиком; ошибка — только в лог.
async fn discard(dir: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(dir:? = dir, error:% = e; "failed to remove a rejected blob");
    }
}

/// Недописанные приёмы прошлого процесса: на старте ядра их уже никто не
/// допишет. Возвращает, сколько записей убрано.
pub async fn sweep(blob_dir: &Path) -> std::io::Result<usize> {
    let staging = blob_dir.join(STAGING);
    tokio::task::spawn_blocking(move || {
        let entries = match std::fs::read_dir(&staging) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let gone = if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())
            } else {
                std::fs::remove_file(entry.path())
            };
            match gone {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(removed)
    })
    .await
    .map_err(std::io::Error::other)?
}

fn expires(now: Timestamp, keep: std::time::Duration) -> Timestamp {
    let nanos = u64::try_from(keep.as_nanos()).unwrap_or(u64::MAX);
    Timestamp::from_nanos(now.as_nanos().saturating_add(nanos))
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Get<C: Components>(
    state: &mut State<C>,
    args: get::Args,
) -> Response<get::Ok, get::Err> {
    let get::Args { id, offset } = args;
    let row = load::<C, get::Err>(state, id).await?;
    state.db_conn.release();

    (offset <= row.size).true_or(Aux::Invalid(format!(
        "offset {offset} is past the end of a {} byte blob",
        row.size
    )))?;
    let chunk = state.shared.cfg().modules.transport.blob_chunk();
    let path = file(&state.shared.cfg().blob_dir(), &row);
    let mut src = tokio::fs::File::open(&path)
        .await
        .map_err(|e| io("open", e))?;
    src.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| io("seek", e))?;

    let meta = get::Item::Meta {
        name: row.name,
        media_type: row.media_type,
        size: row.size,
    };
    let mut left = row.size - offset;
    let items = async_stream::stream! {
        yield Ok(meta);
        while left > 0 {
            let want = usize::try_from(left).unwrap_or(usize::MAX).min(chunk);
            let mut buf = vec![0_u8; want];
            match src.read(&mut buf).await {
                Ok(0) => {
                    yield Err(io("read", std::io::ErrorKind::UnexpectedEof.into()).into());
                    break;
                }
                Ok(n) => {
                    buf.truncate(n);
                    left -= n as u64;
                    yield Ok(get::Item::Chunk(Bytes(buf)));
                }
                Err(e) => {
                    yield Err(io("read", e).into());
                    break;
                }
            }
        }
    };

    Ok(Box::pin(items))
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Stat<C: Components>(
    state: &mut State<C>,
    args: stat::Args,
) -> Response<stat::Ok, stat::Err> {
    let stat::Args { id } = args;
    let row = load::<C, stat::Err>(state, id).await?;
    Ok(convert::blob::meta(row))
}
