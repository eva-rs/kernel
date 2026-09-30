use eva::data;

use crate::types::ModuleId;

#[data(error, display("module {module} has no live token"))]
pub struct NotFound {
    pub module: ModuleId,
}

code!(NotFound => NotFound);

#[data(error, display("module id {module} is taken"))]
pub struct Taken {
    pub module: ModuleId,
}

code!(Taken => Conflict);

#[data(
    error,
    display("module {module} is not a service added by modules.add")
)]
pub struct ServiceNotFound {
    pub module: ModuleId,
}

code!(ServiceNotFound => NotFound);
