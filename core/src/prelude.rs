//! `use eva_core::prelude::*` — шаги и все трейты неймспейсов: без них
//! `session.surfaces().create()` не резолвится.

pub use crate::{
    Caller, Session, SessionMaker,
    service::{
        CallStep, StreamStep, UploadStep, agents::Agents as _, blobs::Blobs as _,
        chats::Chats as _, cron::Cron as _, folders::Folders as _, fs::Fs as _,
        hands_grants::HandsGrants as _, inference::Inference as _, kv::Kv as _,
        memory::Memory as _, models::Models as _, modules::Modules as _, notes::Notes as _,
        persons::Persons as _, sleep::Sleep as _, sqlite::Sqlite as _, stats::Stats as _,
        surfaces::Surfaces as _, tools::Tools as _, turns::Turns as _,
    },
};
