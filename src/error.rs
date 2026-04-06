use presage::store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum SignalNativeError {
    #[error("not linked: run device linking first")]
    NotLinked,

    #[error("presage error: {0}")]
    Presage(String),

    #[error("store error: {0}")]
    Store(String),

    #[error("send failed: {0}")]
    Send(String),

    #[error("invalid recipient: {0}")]
    InvalidRecipient(String),
}

impl<E: StoreError> From<presage::Error<E>> for SignalNativeError {
    fn from(e: presage::Error<E>) -> Self {
        SignalNativeError::Presage(e.to_string())
    }
}
