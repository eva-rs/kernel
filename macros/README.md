# eva-kernel-macros

Единственный список операций библиотеки Евы. Proc-macro `endpoints!` описывает все неймспейсы и операции через [`eva-api-builder`](https://git.desu.church/VienDesu/eva) и порождает трейты неймспейсов и X-макро со списком; по нему `eva-core` собирает `Session`, а `eva-rpc` и `eva-sdk` — диспетчер, клиент и `Kernel`.

## Место в слоях

Зависит только от `eva-api-builder`. Типы не импортирует: порождённый код ссылается на `::eva_protocol::requests::<ns>::<op>::{Args, Ok, Err}`, поэтому раскрывать макро можно только там, где виден `eva-protocol`. Зовёт его один крейт — `eva-core`; остальные берут готовое `eva_core::for_each_op!`.

## Что порождает

`eva_kernel_macros::endpoints!(for_each_op)` в `core/src/service.rs` даёт:

| Что | Форма |
|---|---|
| `service::<ns>::<Ns>` | трейт неймспейса: `fn <op>(&mut self) -> impl CallStep<Args, Output = Response<Ok, Err>>`; `auto_impl` для `&mut` и `Box` |
| `for_each_op!` | `#[macro_export]` X-макро: `for_each_op!(callback)` раскрывает `callback! { <ns> { <ns>: <Ns> { #[flag] <op> = <op> { args, ok, err, output } … } } … }` |

Запись списка — `.service("<ns>", |svc| …)` в порядке неймспейсов; операция — `.method("<op>")`, `.methods([...])` или `.method_with("<op>", |m| m.flag(...))`. Raw-имя (`r#move`) остаётся raw в Rust, RPC-метод — без `r#`.

Флаги:

| Флаг | Вид | Что меняет |
|---|---|---|
| — | чтение | SDK повторяет его после реконнекта с новым id |
| `mutating` | мутация | окно идемпотентности сервера; SDK повторяет только тем же id |
| `stream` | поток к вызывающему | `Ok = Items<Item, Err>`; ответ идёт кадрами потока |
| `upload` | поток от вызывающего | `Args { meta, body: ByteStream }`; в `params` — `Meta`, тело — чанками |

Флаг не сверяет ни один тест: мутация без `mutating` пройдёт мимо окна идемпотентности, и SDK повторит её после реконнекта.

## Пример

Колбэк над списком (форма — как в `core/src/service.rs`):

```rust
macro_rules! methods {
    ($($segment:ident {$($accessor:ident : $Trait:ident {$(
        $(#[$flag:ident])*
        $method:ident = $endpoint:ident {
            args: $Args:ty, ok: $Ok:ty, err: $Err:ty, output: $Output:ty,
        }
    )*})*})*) => {
        // `op_name` снимает `r#`: `chats.move`, не `chats.r#move`.
        pub const METHODS: &[(&str, &str)] = &[$($($(
            (stringify!($segment), eva_core::service::op_name(stringify!($endpoint))),
        )*)*)*];
    };
}

eva_core::for_each_op!(methods);
```

## Тесты

Своих нет: список проверяют потребители. `core/tests/namespaces.rs` — raw-имена без `r#` и `prelude`; `rpc/tests/it/methods.rs` — таблица диспетчера совпадает с `NAMESPACES` по именам и виду; `services/kernel/tests/contract.rs` и `k3_policy_all.rs` — каждая операция в процессе, по проводу и в матрице прав.

## Документация

[Библиотечное API](../docs/src/api.md), [Разработка](../docs/src/development.md#новая-операция).
