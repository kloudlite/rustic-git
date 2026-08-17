pub mod auth;
pub mod gc;
pub mod http;
pub mod pktline;
pub mod pool;
pub mod protocol;
pub mod refs;
pub mod ssh;
pub mod store;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn err(msg: impl Into<String>) -> Error {
    msg.into().into()
}

pub struct App {
    pub store: std::sync::Arc<store::Store>,
}

impl App {
    pub fn new(store: std::sync::Arc<store::Store>) -> Self {
        App { store }
    }
}
