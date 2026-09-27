//! `impl <Namespace> for Session<C>`: строка `project!` на операцию.
//! Операция трейта без строки — ошибка компиляции.

/// `fn get() = h::Get;` → шаг `with_state`. Модуль операции должен быть в
/// области видимости.
macro_rules! project {
    () => {};
    (fn $method:ident() = $handler:expr; $($tail:tt)*) => {
        fn $method(
            &mut self,
        ) -> impl ::eva_core::CallStep<
            $method::Args,
            Output = ::eva_protocol::requests::Response<$method::Ok, $method::Err>,
        > {
            self.with_state($handler)
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
