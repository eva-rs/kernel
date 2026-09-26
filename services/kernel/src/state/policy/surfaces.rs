//! Surface: `Service` владеет только своими (`surface.module = me`), у
//! builtin `local`/`system` модуля нет — ни одному сервису.

use eva_core::Caller;
use eva_protocol::{
    errors::{Aux, AuxResult},
    requests::surfaces::create,
    types::ModuleId,
};

use crate::{
    db::surfaces::{LOCAL, SYSTEM, SurfaceFilter, SurfaceRef},
    state::policy::Policy,
};

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

    /// Фильтр `*.list`: в `WHERE`, не пост-фильтром.
    pub fn visible(self) -> SurfaceFilter {
        match self.0.caller() {
            Caller::Service { module } => SurfaceFilter::Module(module.clone()),
            Caller::Kernel | Caller::Client { .. } => SurfaceFilter::All,
        }
    }

    /// `surfaces.create`: только модуль; `system` — ничей, `local` — только
    /// клиентам. Занятость чужим модулем решает хендлер по БД (`Taken`).
    pub fn create(self, args: &create::Args) -> AuxResult<&'a ModuleId> {
        let module = self.0.module()?;
        if args.id.as_str() == SYSTEM {
            return Err(Aux::forbidden("surface system belongs to the kernel"));
        }
        if args.id.as_str() == LOCAL && matches!(self.0.caller(), Caller::Service { .. }) {
            return Err(Aux::forbidden(
                "surface local is served by client modules only",
            ));
        }
        Ok(module)
    }
}
