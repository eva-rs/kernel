//! `impl <Namespace> for Session<C>`: строка `project!` на операцию.
//! Операция трейта без строки — ошибка компиляции.

/// `fn get() = h::Get;` → шаг `with_state`; `stream fn`/`upload fn` — через
/// `with_stream`/`with_upload`. Модуль операции должен быть в области видимости.
macro_rules! project {
    () => {};
    (fn $method:ident() = $handler:expr; $($tail:tt)*) => {
        fn $method(
            &mut self,
        ) -> impl ::eva_core::CallStep<$method::Args, Ok = $method::Ok, Err = $method::Err> {
            self.with_state($handler)
        }

        project!($($tail)*);
    };
    (stream fn $method:ident() = $handler:expr; $($tail:tt)*) => {
        fn $method(
            &mut self,
        ) -> impl ::eva_core::StreamStep<$method::Args, Item = $method::Item, Err = $method::Err> {
            self.with_stream($handler)
        }

        project!($($tail)*);
    };
    (upload fn $method:ident() = $handler:expr; $($tail:tt)*) => {
        fn $method(
            &mut self,
        ) -> impl ::eva_core::UploadStep<$method::Args, Ok = $method::Ok, Err = $method::Err> {
            self.with_upload($handler)
        }

        project!($($tail)*);
    };
}

mod agents;
mod blobs;
mod chats;
mod cron;
mod folders;
mod fs;
mod hands_grants;
mod inference;
mod kv;
mod memory;
mod models;
mod modules;
mod notes;
mod persons;
mod sleep;
mod sqlite;
mod stats;
mod surfaces;
mod tools;
mod turns;
