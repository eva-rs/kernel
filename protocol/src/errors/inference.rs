use eva::{data, time::HumanDuration};

use crate::{errors::HasCode, types::generate::ProviderError, wire::ErrorCode};

#[data(error, display("provider failed: {error}"))]
pub struct Provider {
    pub error: ProviderError,
}

impl HasCode for Provider {
    fn code(&self) -> ErrorCode {
        self.error.code()
    }

    fn retry_after(&self) -> Option<HumanDuration> {
        self.error.retry_after()
    }
}
