use eva::data;

#[data(error, display("{path} does not exist"))]
pub struct NotFound {
    pub path: String,
}

#[data(error, display("{path} is a directory"))]
pub struct IsDir {
    pub path: String,
}

#[data(error, display("{path} is not a directory"))]
pub struct NotDir {
    pub path: String,
}
