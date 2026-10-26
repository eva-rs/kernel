//! SDK без soul: поддельный soul отвечает на `hello` руками и зовёт модуль
//! настоящим `ModuleClient` — тот же путь, что у soul (`06 §4`); сервис он
//! набирает по uds и tcp под Noise.

mod builder;
mod config;
mod dispatch;
mod fake;
mod listen;
mod reconnect;

mod high_level;
