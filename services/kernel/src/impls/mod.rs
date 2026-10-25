//! Реализации сервисов следуют метаданным протокола и именам обработчиков.

use eva::handling::CallStep;

use crate::{components::Components, handling, session::Session};

macro_rules! decorate {
    ($service:ident, $method:ident; $handler:expr) => {
        $handler
    };
}

macro_rules! project {
    ($(
        $segment:ident { $accessor:ident: $Name:ident {
            trait: $Trait:path,
            client: $Client:path,
            $(
                $(#[$flag:ident])*
                $method:ident = $endpoint:ident {
                    args: $Args:ty,
                    ok: $Ok:ty,
                    err: $Err:ty,
                    output: $Output:ty,
                    docs: [$($doc:literal),*],
                }
            )*
        } }
    )*) => { eva::paste! {
        $(impl<C: Components> $Trait for Session<C> {
            $(fn $method(&mut self) -> impl CallStep<$Args, Output = $Output> {
                self.with_state(decorate!($segment, $method; handling::$segment::[<$endpoint:camel>]))
            })*
        })*
    } };
}

eva_core::for_each_op!(project);
