//! VFS файлов `sqlite.*` модулей (`06 §5`, I12): системный VFS с квотой на
//! временные файлы вызова. `max_page_count` их не видит, а сортировка,
//! DISTINCT, временные b-деревья и журналы операторов пишут в них сколько
//! угодно: без квоты один `sqlite.query` занял бы весь диск.
//!
//! Бюджет берётся из потока, открывающего файл ([`charge_to`]): соединение
//! модуля живёт один вызов в одном блокирующем потоке, рабочих потоков
//! сортировки у него нет (`PRAGMA threads` модулю запрещена). Временный
//! файл вне бюджета не растёт вовсе.

use std::{
    cell::RefCell,
    ffi::{CStr, c_int, c_void},
    mem::offset_of,
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
};

use rusqlite::ffi;

/// Имя VFS для `Connection::open_with_flags_and_vfs`.
pub const NAME: &CStr = c"eva-module";

/// Файлы, которые SQLite заводит сам: временная база, сортировка,
/// временные b-деревья, журналы операторов и временной базы.
const TEMP: c_int = ffi::SQLITE_OPEN_TEMP_DB
    | ffi::SQLITE_OPEN_TEMP_JOURNAL
    | ffi::SQLITE_OPEN_TRANSIENT_DB
    | ffi::SQLITE_OPEN_SUBJOURNAL;

/// Байты временных файлов одного вызова `sqlite.*`.
#[derive(Debug)]
pub struct TempBudget {
    limit: u64,
    used: AtomicU64,
}

impl TempBudget {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicU64::new(0),
        })
    }

    /// Оплачено сейчас: живые временные файлы вызова.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    fn charge(&self, bytes: u64) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|&total| total <= self.limit)
            })
            .is_ok()
    }

    fn refund(&self, bytes: u64) {
        _ = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(bytes))
            });
    }
}

thread_local! {
    static BUDGET: RefCell<Option<Arc<TempBudget>>> = const { RefCell::new(None) };
}

/// Пока жив, временные файлы, открытые этим потоком, платят из бюджета.
#[must_use = "the budget applies only while the guard lives"]
pub struct Charging(Option<Arc<TempBudget>>);

pub fn charge_to(budget: Arc<TempBudget>) -> Charging {
    Charging(BUDGET.with_borrow_mut(|slot| slot.replace(budget)))
}

impl Drop for Charging {
    fn drop(&mut self) {
        let previous = self.0.take();
        BUDGET.with_borrow_mut(|slot| *slot = previous);
    }
}

/// Системный VFS, поверх которого стоит этот.
static SYSTEM: AtomicPtr<ffi::sqlite3_vfs> = AtomicPtr::new(ptr::null_mut());
static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();

/// Регистрирует VFS [`NAME`] один раз на процесс.
pub fn register() -> Result<(), String> {
    REGISTERED
        // SAFETY: регистрация одна на процесс, структура VFS живёт до его конца.
        .get_or_init(|| unsafe { install() })
        .clone()
}

unsafe fn install() -> Result<(), String> {
    // SAFETY: `sqlite3_vfs_find` сам инициализирует библиотеку.
    let system = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
    if system.is_null() {
        return Err(String::from("sqlite has no default VFS"));
    }
    // Копия системного VFS целиком, как у `test_quota.c`: его методы, кроме
    // `xOpen`, получают этот VFS, но читают только скопированные поля.
    // SAFETY: `system` — живой VFS библиотеки.
    let mut vfs = unsafe { *system };
    let header = c_int::try_from(offset_of!(File, inner))
        .map_err(|_| String::from("VFS file header does not fit c_int"))?;
    vfs.szOsFile = vfs
        .szOsFile
        .checked_add(header)
        .ok_or_else(|| String::from("VFS file size overflows c_int"))?;
    vfs.zName = NAME.as_ptr();
    vfs.pNext = ptr::null_mut();
    vfs.xOpen = Some(x_open);
    SYSTEM.store(system, Ordering::Release);
    let vfs = Box::leak(Box::new(vfs));
    // SAFETY: `vfs` живёт до конца процесса; `0` — не VFS по умолчанию.
    match unsafe { ffi::sqlite3_vfs_register(vfs, 0) } {
        ffi::SQLITE_OK => Ok(()),
        rc => Err(format!("sqlite3_vfs_register failed with code {rc}")),
    }
}

/// Открытый файл: заголовок квоты, за ним файл системного VFS.
#[repr(C)]
struct File {
    base: ffi::sqlite3_file,
    /// `Arc::into_raw` бюджета; null — файл не временный или открыт вне бюджета.
    budget: *const TempBudget,
    temp: bool,
    /// Оплаченная длина файла.
    charged: u64,
    /// Начало файла системного VFS длиной его `szOsFile`.
    inner: ffi::sqlite3_file,
}

/// Файл системного VFS внутри нашего.
unsafe fn sub(file: *mut ffi::sqlite3_file) -> *mut ffi::sqlite3_file {
    // SAFETY: SQLite выделил `szOsFile` байт под `File` и системный файл за ним.
    unsafe { ptr::addr_of_mut!((*file.cast::<File>()).inner) }
}

/// Методы системного файла; открытый файл их всегда имеет.
unsafe fn methods(sub: *mut ffi::sqlite3_file) -> &'static ffi::sqlite3_io_methods {
    // SAFETY: `x_open` ставит наши методы только поверх открытого системного файла.
    unsafe { &*(*sub).pMethods }
}

fn current_budget() -> *const TempBudget {
    BUDGET
        .try_with(|slot| slot.try_borrow().ok().and_then(|b| b.clone()))
        .ok()
        .flatten()
        .map_or(ptr::null(), Arc::into_raw)
}

unsafe extern "C" fn x_open(
    _vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let system = SYSTEM.load(Ordering::Acquire);
    let this = file.cast::<File>();
    // SAFETY: `file` — `szOsFile` байт от SQLite; `system` выставлен до регистрации.
    unsafe {
        (*this).base.pMethods = ptr::null();
        let sub = sub(file);
        (*sub).pMethods = ptr::null();
        let Some(open) = (*system).xOpen else {
            return ffi::SQLITE_CANTOPEN;
        };
        let rc = open(system, name, sub, flags, out_flags);
        if rc != ffi::SQLITE_OK {
            if !(*sub).pMethods.is_null()
                && let Some(close) = methods(sub).xClose
            {
                close(sub);
            }
            return rc;
        }
        let temp = flags & TEMP != 0;
        ptr::addr_of_mut!((*this).temp).write(temp);
        ptr::addr_of_mut!((*this).charged).write(0);
        ptr::addr_of_mut!((*this).budget).write(if temp { current_budget() } else { ptr::null() });
        (*this).base.pMethods = if methods(sub).iVersion >= 2 {
            &IO_V2
        } else {
            &IO_V1
        };
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_close(file: *mut ffi::sqlite3_file) -> c_int {
    let this = file.cast::<File>();
    // SAFETY: SQLite зовёт `xClose` один раз на открытый нами файл.
    unsafe {
        let budget = std::mem::replace(&mut (*this).budget, ptr::null());
        if !budget.is_null() {
            let budget = Arc::from_raw(budget);
            budget.refund((*this).charged);
        }
        let sub = sub(file);
        match methods(sub).xClose {
            Some(close) => close(sub),
            None => ffi::SQLITE_OK,
        }
    }
}

/// Оплатить рост временного файла до `end` байт.
unsafe fn grow(file: *mut ffi::sqlite3_file, end: u64) -> bool {
    let this = file.cast::<File>();
    // SAFETY: `file` открыт `x_open`, бюджет жив, пока файл не закрыт.
    unsafe {
        if !(*this).temp || end <= (*this).charged {
            return true;
        }
        let budget = (*this).budget;
        if budget.is_null() || !(*budget).charge(end - (*this).charged) {
            return false;
        }
        (*this).charged = end;
    }
    true
}

unsafe extern "C" fn x_write(
    file: *mut ffi::sqlite3_file,
    buf: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let end = u64::try_from(offset)
        .unwrap_or(0)
        .saturating_add(u64::try_from(amount).unwrap_or(0));
    // SAFETY: файл открыт `x_open`.
    unsafe {
        if !grow(file, end) {
            return ffi::SQLITE_FULL;
        }
        let sub = sub(file);
        match methods(sub).xWrite {
            Some(write) => write(sub, buf, amount, offset),
            None => ffi::SQLITE_IOERR_WRITE,
        }
    }
}

unsafe extern "C" fn x_truncate(file: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    let size = u64::try_from(size).unwrap_or(0);
    let this = file.cast::<File>();
    // SAFETY: файл открыт `x_open`.
    unsafe {
        if !grow(file, size) {
            return ffi::SQLITE_FULL;
        }
        if (*this).temp && size < (*this).charged {
            if !(*this).budget.is_null() {
                (*(*this).budget).refund((*this).charged - size);
            }
            (*this).charged = size;
        }
        let sub = sub(file);
        match methods(sub).xTruncate {
            Some(truncate) => truncate(sub, size.try_into().unwrap_or(ffi::sqlite3_int64::MAX)),
            None => ffi::SQLITE_IOERR_TRUNCATE,
        }
    }
}

unsafe extern "C" fn x_file_control(
    file: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    // SAFETY: файл открыт `x_open`.
    unsafe {
        // Подсказка размера растит файл мимо `xWrite`: временному она не нужна.
        if (*file.cast::<File>()).temp
            && matches!(
                op,
                ffi::SQLITE_FCNTL_SIZE_HINT | ffi::SQLITE_FCNTL_CHUNK_SIZE
            )
        {
            return ffi::SQLITE_OK;
        }
        let sub = sub(file);
        match methods(sub).xFileControl {
            Some(control) => control(sub, op, arg),
            None => ffi::SQLITE_NOTFOUND,
        }
    }
}

/// Методы, которые только передают вызов системному файлу.
macro_rules! forward {
    ($($name:ident => $field:ident($($arg:ident: $ty:ty),*) -> $ret:ty = $missing:expr;)*) => {
        $(
            unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file $(, $arg: $ty)*) -> $ret {
                // SAFETY: файл открыт `x_open`.
                unsafe {
                    let sub = sub(file);
                    match methods(sub).$field {
                        Some(method) => method(sub $(, $arg)*),
                        None => $missing,
                    }
                }
            }
        )*
    };
}

forward! {
    x_read => xRead(buf: *mut c_void, amount: c_int, offset: ffi::sqlite3_int64) -> c_int = ffi::SQLITE_IOERR_READ;
    x_sync => xSync(flags: c_int) -> c_int = ffi::SQLITE_OK;
    x_file_size => xFileSize(size: *mut ffi::sqlite3_int64) -> c_int = ffi::SQLITE_IOERR_FSTAT;
    x_lock => xLock(level: c_int) -> c_int = ffi::SQLITE_OK;
    x_unlock => xUnlock(level: c_int) -> c_int = ffi::SQLITE_OK;
    x_check_reserved_lock => xCheckReservedLock(out: *mut c_int) -> c_int = ffi::SQLITE_OK;
    x_sector_size => xSectorSize() -> c_int = 4096;
    x_device_characteristics => xDeviceCharacteristics() -> c_int = 0;
    x_shm_map => xShmMap(page: c_int, size: c_int, extend: c_int, out: *mut *mut c_void) -> c_int = ffi::SQLITE_IOERR_SHMMAP;
    x_shm_lock => xShmLock(offset: c_int, n: c_int, flags: c_int) -> c_int = ffi::SQLITE_IOERR_SHMLOCK;
    x_shm_unmap => xShmUnmap(delete: c_int) -> c_int = ffi::SQLITE_OK;
}

unsafe extern "C" fn x_shm_barrier(file: *mut ffi::sqlite3_file) {
    // SAFETY: файл открыт `x_open`.
    unsafe {
        let sub = sub(file);
        if let Some(barrier) = methods(sub).xShmBarrier {
            barrier(sub);
        }
    }
}

/// Без `xFetch`: mmap модулю не включить (`PRAGMA mmap_size` — только чтение).
static IO_V2: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_file_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_unlock),
    xCheckReservedLock: Some(x_check_reserved_lock),
    xFileControl: Some(x_file_control),
    xSectorSize: Some(x_sector_size),
    xDeviceCharacteristics: Some(x_device_characteristics),
    xShmMap: Some(x_shm_map),
    xShmLock: Some(x_shm_lock),
    xShmBarrier: Some(x_shm_barrier),
    xShmUnmap: Some(x_shm_unmap),
    xFetch: None,
    xUnfetch: None,
};

static IO_V1: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    ..IO_V2
};
