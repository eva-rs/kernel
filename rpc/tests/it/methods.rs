//! Покрытие (`11 §8`): RPC-метод есть у каждой операции библиотеки, с той же формой шага.

use eva_core::{NAMESPACES, OpKind};
use eva_rpc::Dispatcher;

use crate::kernel::Kernel;

#[test]
fn dispatcher_methods_equal_namespaces() {
    let expected: Vec<(String, OpKind)> = NAMESPACES
        .iter()
        .flat_map(|(ns, ops)| {
            ops.iter()
                .map(move |op| (format!("{ns}.{}", op.name), op.kind))
        })
        .collect();

    assert_eq!(Dispatcher::<Kernel>::methods(), expected);
    assert!(expected.iter().any(|(m, _)| m == "chats.move"));
}
