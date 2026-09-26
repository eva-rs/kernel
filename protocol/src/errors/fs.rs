use eva::data;

#[data(error, display("{path} does not exist"))]
pub struct NotFound {
    pub path: String,
}

code!(NotFound => NotFound);

#[data(error, display("{path} is a directory"))]
pub struct IsDir {
    pub path: String,
}

code!(IsDir => InvalidParams);

#[data(error, display("{path} is not a directory"))]
pub struct NotDir {
    pub path: String,
}

code!(NotDir => InvalidParams);
