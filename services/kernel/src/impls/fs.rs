use eva_core::service::fs::Fs;
use eva_protocol::requests::fs::{list, read, remove, stat, write};

use crate::{components::Components, handling::fs as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Fs for Session<C> { project! {
    fn read() = h::Read;
    fn write() = h::Write;
    fn list() = h::List;
    fn stat() = h::Stat;
    fn remove() = h::Remove;
}}
