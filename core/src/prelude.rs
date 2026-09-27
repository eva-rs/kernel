//! `use eva_core::prelude::*` — шаг и все трейты неймспейсов: без них
//! `session.surfaces().create()` не резолвится.

pub use crate::{
    Caller, Session, SessionMaker,
    service::{CallStep, traits::*},
};
