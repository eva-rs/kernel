//! Компонент `turns` (`01`, `07`): лог сегмента, детерминированный рендер,
//! кэш промпта и счёт токенов.

pub mod api_tools;
pub mod blob;
pub mod cache;
pub mod compaction;
pub mod ctx;
pub mod dedup;
pub mod dispatch;
pub mod driver;
pub mod engine;
pub mod estimate;
pub mod feed;
pub mod gates;
pub mod hooks;
pub mod infer;
pub mod ingest;
pub mod integration;
pub mod items;
pub mod keep_alive;
pub mod kernel_tools;
pub mod live;
pub mod log;
pub mod marker;
pub mod pending;
pub mod plan;
pub mod ports;
pub mod recovery;
pub mod reminder;
pub mod render;
pub mod secrets;
pub mod sleep;
pub mod snapshot;
