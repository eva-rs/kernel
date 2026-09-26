use eva::data;

use crate::types::generate::ProviderError;

#[data(error, display("provider failed: {error}"))]
pub struct Provider {
    pub error: ProviderError,
}
