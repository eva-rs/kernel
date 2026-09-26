use eva_core::service::blobs::Blobs;
use eva_protocol::requests::blobs::{get, put, stat};

use crate::{components::Components, handling::blobs as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Blobs for Session<C> { project! {
    upload fn put() = h::Put;
    stream fn get() = h::Get;
    fn stat() = h::Stat;
}}
