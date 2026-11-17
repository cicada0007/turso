use std::any::Any;
use std::sync::Arc;

use crate::Connection;

pub type SavepointMemo = Arc<dyn Any + Send + Sync>;

/// A value that the owner of a connection stores on it.
pub type ContextValue = Arc<dyn Any + Send + Sync>;

pub trait ConnectionHooks: Any + Send + Sync {
    fn root_statement_ended(&self, conn: &Connection, failed: bool);
    fn transaction_rolled_back(&self, conn: &Connection);
    fn savepoint_created(&self, conn: &Connection) -> SavepointMemo;
    fn rolled_back_to_savepoint(&self, conn: &Connection, memo: &SavepointMemo);
}

/// The state that the owner of a connection gives it when the connection is created.
#[derive(Default)]
pub enum ConnectionContext {
    #[default]
    Empty,
    /// An arbitrary value to store on the Connection. Can be used for session
    /// or transaction storage.
    Value(ContextValue),
    /// A value to store on the Connection that the connection also calls as
    /// its hooks.
    Hooks(Arc<dyn ConnectionHooks>),
}

impl ConnectionContext {
    /// True if the context holds hooks of type `C`.
    pub fn holds_hooks<C: Any>(&self) -> bool {
        match self {
            Self::Hooks(hooks) => (hooks.as_ref() as &dyn Any).is::<C>(),
            Self::Empty | Self::Value(_) => false,
        }
    }

    /// The value that [`Connection::context`] returns, and the hooks the
    /// connection calls.
    pub(crate) fn into_parts(self) -> (Option<ContextValue>, Option<Arc<dyn ConnectionHooks>>) {
        match self {
            Self::Empty => (None, None),
            Self::Value(value) => (Some(value), None),
            Self::Hooks(hooks) => (Some(hooks.clone()), Some(hooks)),
        }
    }
}
