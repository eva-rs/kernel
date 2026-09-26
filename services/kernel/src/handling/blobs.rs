//! Хендлеры `blobs.*` (`07 §6`, `06 §5`): байты в `work.blob_dir/<id>/<name>`,
//! строка — в `blobs`. `Service` видит только blob'ы чатов своих surface.

use std::path::{Path, PathBuf};

use eva::time::Timestamp;
use eva_core::Items;
use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Response,
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
        Db as _, Queries as _,
        blobs::{BlobRow, Source},
    },
    handling::Upload,
    state::{Policy, State},
};

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
    args: Upload<put::Args>,
) -> Response<put::Ok, put::Err> {
    let State {
        shared,
        world,
        caller,
        db_conn,
        ..
    } = state;
    let Upload { args, mut body } = args;
    let put::Args {
        chat,
        name,
        media_type,
        kind,
        size,
        hash,
    } = args;
    let policy = Policy::new(caller);

    let db = db_conn.acquire(shared).await?;
    let chat_ref = db
        .run(move |c| c.chats().chat_ref(chat))
        .await?
        .ok_or_else(|| Aux::NotFound(format!("chat {chat}")))?;
    policy.chats().read(&chat_ref)?;

    if !check_name(&name) {
        return Err(errors::blobs::BadName { name }.spec());
    }
    (!media_type.trim().is_empty() && !media_type.chars().any(char::is_control)).true_or(
        Aux::Invalid("media_type must be a non-empty single line".into()),
    )?;
    let max = shared.cfg().work.blob_max.as_u64();
    (size <= max).true_or(Aux::Quota(format!(
        "blob is {size} bytes, work.blob_max is {max}"
    )))?;
    let declared = hash.as_deref().map(parse_hash).transpose()?;

    let now = world.now();
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
        expires_at: expires(now, *shared.cfg().work.blob_keep),
    };
    let dir = shared.cfg().blob_dir().join(row.id.as_str());

    let received = receive(&dir, &row.name, &mut body, size).await;
    let (actual, digest) = match received {
        Ok(done) => done,
        Err(e) => {
            discard(&dir).await;
            return Err(e);
        }
    };
    let checked = if actual != size {
        Err(errors::blobs::SizeMismatch {
            declared: size,
            actual,
        }
        .spec())
    } else {
        match declared {
            Some(declared) if declared != *digest.as_bytes() => Err(errors::blobs::HashMismatch {
                declared: convert::blob::hex(&declared),
                actual: digest.to_hex().to_string(),
            }
            .spec()),
            _ => Ok(()),
        }
    };
    if let Err(e) = checked {
        discard(&dir).await;
        return Err(e);
    }

    let row = BlobRow {
        hash: digest.as_bytes().to_vec(),
        ..row
    };
    let saved = row.clone();
    if let Err(e) = shared.db().write(move |tx| tx.blobs().insert(&saved)).await {
        discard(&dir).await;
        return Err(Aux::from(e).into());
    }

    log::info!(blob:% = row.id, chat:% = chat, size, caller = caller.label(); "blob stored");
    Ok(put::Ok {
        id: row.id,
        size,
        hash: digest.to_hex().to_string(),
    })
}

/// Приём тела с проверкой по ходу: перебор заявленного размера рвёт приём.
async fn receive(
    dir: &Path,
    name: &str,
    body: &mut eva_core::ByteStream,
    declared: u64,
) -> Response<(u64, blake3::Hash), put::Err> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| io("create dir", e))?;
    let mut out = tokio::fs::File::create(dir.join(name))
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

fn expires(now: Timestamp, keep: std::time::Duration) -> Timestamp {
    let nanos = u64::try_from(keep.as_nanos()).unwrap_or(u64::MAX);
    Timestamp::from_nanos(now.as_nanos().saturating_add(nanos))
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Get<C: Components>(
    state: &mut State<C>,
    args: get::Args,
) -> Response<Items<get::Item, get::Err>, get::Err> {
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
