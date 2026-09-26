//! Таблицы `tools`, `tool_groups`, `tool_placements`, `tool_overrides`, `tool_pins`, `chat_warm_sets`, `chat_loaded_tools` (`02`, `10 §2.2`).

use rusqlite::Connection;

pub struct Tools<'c>(pub &'c Connection);
