use eva::data;

use crate::types::RunId;

#[data(error, display("sleep run {run} does not exist"))]
pub struct RunNotFound {
    pub run: RunId,
}

code!(RunNotFound => NotFound);
