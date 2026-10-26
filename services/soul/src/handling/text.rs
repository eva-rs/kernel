//! Потолки строк, которые вызывающий пишет в базу soul: без них сервис
//! раздувал бы `eva.sqlite`, FTS и системный промпт кадрами до `max_frame`.

use eva_protocol::errors::{Aux, AuxResult, ext::*};

/// Имя или заголовок, символов: чат, surface, папка, личность, handle.
pub const NAME_MAX: usize = 256;
/// Внешний id чата или личности, байт.
pub const EXTERNAL_ID_MAX: usize = 256;
/// `project` и `workdir` чата, байт.
pub const PATH_MAX: usize = 4096;
/// `rules` surface, байт: они входят в каждый системный промпт его чатов.
pub const RULES_MAX: usize = 16 * 1024;
/// `media_type` blob'а, байт.
pub const MEDIA_TYPE_MAX: usize = 255;

/// Не длиннее `max` символов.
pub fn chars(what: &str, value: &str, max: usize) -> AuxResult<()> {
    value
        .chars()
        .nth(max)
        .is_none()
        .true_or(Aux::Invalid(format!("{what} exceeds {max} characters")))
}

/// Не длиннее `max` байт UTF-8.
pub fn bytes(what: &str, value: &str, max: usize) -> AuxResult<()> {
    (value.len() <= max).true_or(Aux::Invalid(format!("{what} exceeds {max} bytes")))
}
