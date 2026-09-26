//! Транскрипт: `segments`, `events`, `messages`, `tool_results`, `token_ratios` (`01`, `10 §2.1`).

use rusqlite::Connection;

pub struct Turns<'c>(pub &'c Connection);
