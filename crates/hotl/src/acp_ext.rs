pub mod codes {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const AUTH_REQUIRED: i64 = -32000;
    pub const NOT_FOUND: i64 = -32002;
}

pub const EXT_PREFIX: &str = "_hotl/";

pub const META_EXT: &str = "hotl.dev/extensions";

pub fn ext(method: &str) -> String {
    format!("{EXT_PREFIX}{method}")
}
