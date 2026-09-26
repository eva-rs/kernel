use eva::data;

#[data(error, display("statement {statement}: {message}"))]
pub struct Sql {
    /// Номер оператора в `tx`; 0 у одиночного.
    pub statement: u32,
    pub message: String,
}
