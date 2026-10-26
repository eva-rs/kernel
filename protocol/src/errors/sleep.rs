use eva::data;

use crate::types::{ModelId, RunId};

#[data(error, display("sleep run {run} does not exist"))]
pub struct RunNotFound {
    pub run: RunId,
}

code!(RunNotFound => NotFound);

#[data(error, display("sleep run {run} is already running"))]
pub struct AlreadyRunning {
    pub run: RunId,
}

code!(AlreadyRunning => Conflict);

#[data(error, display("sleep run {run} is not running"))]
pub struct NotRunning {
    pub run: RunId,
}

code!(NotRunning => Conflict);

#[data(error, display("no provider for sleep model {model}"))]
pub struct NoProvider {
    pub model: ModelId,
}

code!(NoProvider => Unavailable);
