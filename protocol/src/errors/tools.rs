use eva::data;

#[data(error, display("tool path {path} does not exist"))]
pub struct PathNotFound {
    pub path: String,
}

code!(PathNotFound => NotFound);
