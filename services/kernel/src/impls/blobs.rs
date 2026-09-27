use eva_core::service::blobs::Blobs;
use eva_protocol::requests::blobs::{get, put, stat};

use crate::{components::Components, handling::blobs as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Blobs for Session<C> { project! {
    fn put() = h::Put;
    fn get() = h::Get;
    fn stat() = h::Stat;
}}
