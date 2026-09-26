//! Хендлеры `fs.*` (`06 §5`, I12): файлы модуля в
//! `<state_dir>/modules/<module>/fs`. Путь — только относительный и внутри
//! корня: `..`, абсолютный путь и симлинк наружу → `Aux::Forbidden`.

use std::{
    ffi::OsStr,
    fs, io,
    num::NonZeroU64,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use eva::time::Timestamp;
use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Response,
        fs::{Entry, EntryKind, list, read, remove, stat, write},
    },
    types::{Bytes, ModuleId},
};

use crate::{
    components::Components,
    state::{Policy, State},
};

/// Каталог состояния модуля: `<state_dir>/modules/<module>`; в нём `fs/` и
/// `state.sqlite`.
pub fn module_dir(state_dir: &Path, module: &ModuleId) -> AuxResult<PathBuf> {
    let name = module.as_str();
    let ok = !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']);
    ok.true_or(Aux::forbidden(format_args!(
        "module name {name:?} is not a directory name"
    )))?;
    Ok(state_dir.join("modules").join(name))
}

/// Отказ внутри блокирующей части; путь операции знает хендлер.
#[derive(Debug)]
enum Fail {
    NotFound,
    IsDir,
    NotDir,
    Aux(Aux),
}

impl From<Aux> for Fail {
    fn from(aux: Aux) -> Self {
        Self::Aux(aux)
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::IsADirectory => Self::IsDir,
            io::ErrorKind::NotADirectory => Self::NotDir,
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
                Self::Aux(Aux::Quota(format!("module fs: {e}")))
            }
            _ => Self::Aux(Aux::internal(format_args!("module fs: {e}"))),
        }
    }
}

type FsResult<T> = Result<T, Fail>;

/// Корень `fs` модуля, канонический: с ним сравниваются цели симлинков.
struct Jail {
    root: PathBuf,
}

impl Jail {
    fn open(dir: &Path) -> FsResult<Self> {
        let root = dir.join("fs");
        fs::create_dir_all(&root)?;
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    /// Путь внутри корня. `follow_last = false` оставляет последний
    /// компонент как есть: `remove` удаляет саму ссылку, не цель.
    fn resolve(&self, rel: &str, follow_last: bool) -> FsResult<PathBuf> {
        let parts = parse(rel)?;
        let mut cur = self.root.clone();
        let mut missing = false;
        for (i, part) in parts.iter().enumerate() {
            let next = cur.join(part);
            let last = i + 1 == parts.len();
            if missing || (last && !follow_last) {
                cur = next;
                continue;
            }
            match fs::symlink_metadata(&next) {
                Ok(meta) if meta.file_type().is_symlink() => cur = self.inside(&next)?,
                Ok(_) => cur = next,
                // Дальше пути нет — симлинков в нём тоже.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    missing = true;
                    cur = next;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(cur)
    }

    /// Цель симлинка, если она внутри корня.
    fn inside(&self, link: &Path) -> FsResult<PathBuf> {
        match link.canonicalize() {
            Ok(target) if target.starts_with(&self.root) => Ok(target),
            _ => Err(Aux::forbidden("symlink leads outside the module directory").into()),
        }
    }

    /// Занято файлами под корнем; симлинки не разыменовываются.
    fn usage(&self) -> FsResult<u64> {
        let mut total = 0_u64;
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let meta = entry.path().symlink_metadata()?;
                if meta.is_dir() {
                    stack.push(entry.path());
                } else {
                    total = total.saturating_add(meta.len());
                }
            }
        }
        Ok(total)
    }

    fn entry(&self, name: String, path: &Path) -> FsResult<Entry> {
        let meta = fs::metadata(path)?;
        Ok(Entry {
            name,
            kind: if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: timestamp(meta.modified().ok()),
        })
    }
}

/// Лексическая проверка: только обычные компоненты и `.`.
fn parse(rel: &str) -> FsResult<Vec<&OsStr>> {
    if rel.contains('\0') {
        return Err(Aux::Invalid("path contains NUL".into()).into());
    }
    Path::new(rel)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| match c {
            Component::Normal(part) => Ok(part),
            Component::ParentDir => {
                Err(Aux::forbidden("path leaves the module directory via ..").into())
            }
            _ => Err(Aux::forbidden("path must be relative to the module directory").into()),
        })
        .collect()
}

fn timestamp(at: Option<SystemTime>) -> Timestamp {
    at.and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .and_then(NonZeroU64::new)
        .map_or(Timestamp::MIN, Timestamp::from_nanos)
}

/// Блокирующая часть операции в пуле `spawn_blocking`.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> FsResult<T> + Send + 'static,
) -> FsResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Fail::Aux(Aux::internal(format_args!("fs task failed: {e}"))))?
}

/// Корень модуля сессии: `Policy` — `module()` (`11 §4`).
fn jail_dir<C: Components>(state: &State<C>) -> AuxResult<PathBuf> {
    let module = Policy::new(&state.caller).module()?;
    module_dir(&state.shared.cfg().system.state_dir, module)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Read<C: Components>(
    state: &mut State<C>,
    args: read::Args,
) -> Response<read::Ok, read::Err> {
    let dir = jail_dir(state)?;
    let read::Args { path } = args;
    let max = state.shared.cfg().modules.transport.max_frame.as_u64();

    let rel = path.clone();
    let res = blocking(move || {
        let jail = Jail::open(&dir)?;
        let full = jail.resolve(&rel, true)?;
        let meta = fs::metadata(&full)?;
        if meta.is_dir() {
            return Err(Fail::IsDir);
        }
        (meta.len() <= max).true_or(Aux::Quota(format!(
            "file is {} bytes, a frame carries at most {max}",
            meta.len()
        )))?;
        Ok(fs::read(&full)?)
    })
    .await;

    match res {
        Ok(data) => Ok(read::Ok { data: Bytes(data) }),
        Err(Fail::NotFound | Fail::NotDir) => Err(errors::fs::NotFound { path }.spec()),
        Err(Fail::IsDir) => Err(errors::fs::IsDir { path }.spec()),
        Err(Fail::Aux(aux)) => Err(aux.into()),
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Write<C: Components>(
    state: &mut State<C>,
    args: write::Args,
) -> Response<write::Ok, write::Err> {
    let dir = jail_dir(state)?;
    let write::Args {
        path,
        data,
        create_dirs,
    } = args;
    let quota = state.shared.cfg().modules.fs_quota.as_u64();

    let rel = path.clone();
    let res = blocking(move || {
        let jail = Jail::open(&dir)?;
        if parse(&rel)?.is_empty() {
            return Err(Fail::IsDir);
        }
        let full = jail.resolve(&rel, true)?;
        let old = match fs::metadata(&full) {
            Ok(meta) if meta.is_dir() => return Err(Fail::IsDir),
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };

        let size = data.len() as u64;
        let used = jail.usage()?.saturating_sub(old);
        (used.saturating_add(size) <= quota).true_or(Aux::Quota(format!(
            "module fs quota is {quota} bytes, {used} are used"
        )))?;

        if let Some(parent) = full.parent() {
            match fs::metadata(parent) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(Fail::NotDir),
                Err(e) if e.kind() == io::ErrorKind::NotFound && create_dirs => {
                    fs::create_dir_all(parent)?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        fs::write(&full, &data)?;
        Ok(size)
    })
    .await;

    match res {
        Ok(size) => Ok(write::Ok { size }),
        Err(Fail::NotFound) => Err(errors::fs::NotFound { path }.spec()),
        Err(Fail::IsDir) => Err(errors::fs::IsDir { path }.spec()),
        Err(Fail::NotDir) => {
            Err(Aux::Invalid(format!("a parent of {path} is a file, not a directory")).into())
        }
        Err(Fail::Aux(aux)) => Err(aux.into()),
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn List<C: Components>(
    state: &mut State<C>,
    args: list::Args,
) -> Response<list::Ok, list::Err> {
    let dir = jail_dir(state)?;
    let list::Args { path } = args;

    let rel = path.clone();
    let res = blocking(move || {
        let jail = Jail::open(&dir)?;
        let full = jail.resolve(&rel, true)?;
        if !fs::metadata(&full)?.is_dir() {
            return Err(Fail::NotDir);
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&full)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut target = entry.path();
            if entry.file_type()?.is_symlink() {
                // Ссылка наружу не показывается: её не прочесть и не записать.
                match jail.inside(&target) {
                    Ok(inner) => target = inner,
                    Err(_) => continue,
                }
            }
            entries.push(jail.entry(name, &target)?);
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    })
    .await;

    match res {
        Ok(entries) => Ok(list::Ok { entries }),
        Err(Fail::NotFound) => Err(errors::fs::NotFound { path }.spec()),
        Err(Fail::NotDir | Fail::IsDir) => Err(errors::fs::NotDir { path }.spec()),
        Err(Fail::Aux(aux)) => Err(aux.into()),
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Stat<C: Components>(
    state: &mut State<C>,
    args: stat::Args,
) -> Response<stat::Ok, stat::Err> {
    let dir = jail_dir(state)?;
    let stat::Args { path } = args;

    let rel = path.clone();
    let res = blocking(move || {
        let jail = Jail::open(&dir)?;
        let name = parse(&rel)?
            .last()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let full = jail.resolve(&rel, true)?;
        jail.entry(name, &full)
    })
    .await;

    match res {
        Ok(entry) => Ok(stat::Ok { entry }),
        Err(Fail::NotFound | Fail::NotDir) => Err(errors::fs::NotFound { path }.spec()),
        Err(Fail::IsDir) => Err(Aux::internal("stat reported a directory as an error").into()),
        Err(Fail::Aux(aux)) => Err(aux.into()),
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Remove<C: Components>(
    state: &mut State<C>,
    args: remove::Args,
) -> Response<remove::Ok, remove::Err> {
    let dir = jail_dir(state)?;
    let remove::Args { path, recursive } = args;

    let rel = path.clone();
    let res = blocking(move || {
        let jail = Jail::open(&dir)?;
        if parse(&rel)?.is_empty() {
            return Err(Aux::Invalid("the module root cannot be removed".into()).into());
        }
        let full = jail.resolve(&rel, false)?;
        let meta = fs::symlink_metadata(&full)?;
        if !meta.is_dir() {
            fs::remove_file(&full)?;
        } else if recursive {
            fs::remove_dir_all(&full)?;
        } else {
            fs::remove_dir(&full).map_err(|e| match e.kind() {
                io::ErrorKind::DirectoryNotEmpty => Fail::Aux(Aux::Invalid(
                    "directory is not empty; pass recursive".into(),
                )),
                _ => e.into(),
            })?;
        }
        Ok(())
    })
    .await;

    match res {
        Ok(()) => Ok(remove::Ok {}),
        Err(Fail::NotFound | Fail::NotDir) => Err(errors::fs::NotFound { path }.spec()),
        Err(Fail::IsDir) => Err(Aux::internal("remove reported a directory as an error").into()),
        Err(Fail::Aux(aux)) => Err(aux.into()),
    }
}
