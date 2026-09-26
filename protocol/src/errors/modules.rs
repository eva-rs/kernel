use eva::data;

use crate::types::ModuleId;

#[data(error, display("module {module} has no live token"))]
pub struct NotFound {
    pub module: ModuleId,
}

#[data(error, display("module name {module} is reserved"))]
pub struct Reserved {
    pub module: ModuleId,
}
