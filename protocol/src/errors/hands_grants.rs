use eva::data;

use crate::types::HandsGrantId;

#[data(error, display("hands grant {id} does not exist"))]
pub struct NotFound {
    pub id: HandsGrantId,
}

code!(NotFound => NotFound);
