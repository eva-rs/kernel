//! Surface: `Service` владеет только своими (`surface.module = me`), у
//! builtin `local`/`system` модуля нет — ни одному сервису.

use std::fmt::Display;

use eva_core::Caller;
use eva_protocol::{
    errors::{Aux, AuxResult},
    requests::surfaces::create,
    types::ModuleId,
};

use crate::{
    config::chats::MasterIdentity,
    db::{
        folders::FolderRow,
        surfaces::{LOCAL, SYSTEM, SurfaceFilter, SurfaceRef},
    },
    state::policy::Policy,
};

/// Объект, чьи права решает его surface.
pub trait OnSurface {
    fn surface(&self) -> &SurfaceRef;
}

impl<T: OnSurface> OnSurface for &T {
    fn surface(&self) -> &SurfaceRef {
        (**self).surface()
    }
}

impl OnSurface for SurfaceRef {
    fn surface(&self) -> &SurfaceRef {
        self
    }
}

impl OnSurface for FolderRow {
    fn surface(&self) -> &SurfaceRef {
        &self.surface
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SurfacesPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn surfaces(self) -> SurfacesPolicy<'a> {
        SurfacesPolicy(self)
    }
}

impl<'a> SurfacesPolicy<'a> {
    /// Объект с `S` в §3: surface, surface чата, pending, blob.
    pub fn own(self, surface: &SurfaceRef) -> AuxResult<()> {
        match self.0.caller() {
            Caller::Kernel | Caller::Client { .. } => Ok(()),
            Caller::Service { module } if surface.module.as_ref() == Some(module) => Ok(()),
            Caller::Service { .. } => Err(Aux::forbidden(format_args!(
                "surface {} belongs to another module",
                surface.id
            ))),
        }
    }

    /// Объект из аргумента (`None` — не найден). `Service` получает один и
    /// тот же отказ за чужой и несуществующий: иначе отказ выдавал бы, что
    /// есть у хозяина на чужих surface.
    pub fn named<T: OnSurface>(self, what: impl Display, found: Option<T>) -> AuxResult<T> {
        match (self.0.caller(), found) {
            (Caller::Service { module }, Some(obj))
                if obj.surface().module.as_ref() == Some(module) =>
            {
                Ok(obj)
            }
            (Caller::Service { .. }, _) => Err(Aux::forbidden(format_args!(
                "{what} is not owned by this module"
            ))),
            (Caller::Kernel | Caller::Client { .. }, Some(obj)) => Ok(obj),
            (Caller::Kernel | Caller::Client { .. }, None) => Err(Aux::NotFound(what.to_string())),
        }
    }

    /// Фильтр `*.list`: в `WHERE`, не пост-фильтром.
    pub fn visible(self) -> SurfaceFilter {
        match self.0.caller() {
            Caller::Service { module } => SurfaceFilter::Module(module.clone()),
            Caller::Kernel | Caller::Client { .. } => SurfaceFilter::All,
        }
    }

    /// `surfaces.create`: только модуль; builtin `local`/`system` сеет ядро —
    /// никому, surface адреса хозяина — только модулю, привязанному к нему в
    /// `chats.persons.master`. Занятость чужим модулем решает хендлер по БД (`Taken`).
    pub fn create(
        self,
        args: &create::Args,
        masters: &[MasterIdentity],
    ) -> AuxResult<&'a ModuleId> {
        let module = self.0.module()?;
        if [LOCAL, SYSTEM].contains(&args.id.as_str()) {
            return Err(Aux::forbidden(format_args!(
                "surface {} is builtin: the kernel seeds it",
                args.id
            )));
        }
        if let Some(bound) = masters
            .iter()
            .filter(|pair| pair.surface == args.id)
            .find_map(|pair| pair.module.as_ref().filter(|bound| *bound != module))
        {
            return Err(Aux::forbidden(format_args!(
                "surface {} is bound to module {bound} by chats.persons.master",
                args.id
            )));
        }
        Ok(module)
    }
}
