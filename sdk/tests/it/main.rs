//! SDK без ядра: поддельное ядро отвечает на `hello` руками и зовёт модуль
//! настоящим `ModuleClient` — тот же путь, что у ядра (`06 §4`).

mod builder;
mod config;
mod dispatch;
mod fake;
mod reconnect;
