//! Кто сейчас обслуживает surface и просил ли кнопки (`ask`, `05 §1.1`,
//! `07 §1`). В БД не пишется: состояние живых сессий; модуль вытесняется
//! целиком при отключении.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

use eva_protocol::types::{ModuleId, SurfaceId};

#[derive(Debug, Default)]
pub struct Serving(Mutex<BTreeMap<SurfaceId, BTreeMap<ModuleId, bool>>>);

impl Serving {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<SurfaceId, BTreeMap<ModuleId, bool>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Сессия модуля взялась обслуживать surface: создатель или `builtin`-клиент `local`.
    pub fn serve(&self, surface: &SurfaceId, module: &ModuleId, ask: bool) {
        self.lock()
            .entry(surface.clone())
            .or_default()
            .insert(module.clone(), ask);
    }

    /// Сессия обслуживает surface; прежний `ask`, если был, сохраняется.
    pub fn enlist(&self, surface: &SurfaceId, module: &ModuleId) {
        self.lock()
            .entry(surface.clone())
            .or_default()
            .entry(module.clone())
            .or_insert(false);
    }

    /// Новый `ask` обслуживающей сессии; не обслуживает — ничего.
    pub fn set_ask(&self, surface: &SurfaceId, module: &ModuleId, ask: bool) -> bool {
        match self
            .lock()
            .get_mut(surface)
            .and_then(|modules| modules.get_mut(module))
        {
            Some(slot) => {
                *slot = ask;
                true
            }
            None => false,
        }
    }

    pub fn serves(&self, surface: &SurfaceId, module: &ModuleId) -> bool {
        self.lock()
            .get(surface)
            .is_some_and(|modules| modules.contains_key(module))
    }

    /// `ask_ui` чатов surface: хоть одна живая обслуживающая сессия просила кнопки.
    pub fn ask_ui(&self, surface: &SurfaceId) -> bool {
        self.lock()
            .get(surface)
            .is_some_and(|modules| modules.values().any(|&ask| ask))
    }

    /// Сессия модуля закрылась.
    pub fn drop_module(&self, module: &ModuleId) {
        let mut surfaces = self.lock();
        surfaces.retain(|_, modules| {
            modules.remove(module);
            !modules.is_empty()
        });
    }
}
