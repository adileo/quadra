//! (to be written)

use super::{Ask, Failed, Fetched, Put, Resolved};

pub async fn fetch(c: &Resolved, _ask: &Ask, _public_only: bool) -> Result<Fetched, Failed> {
    Err(Failed::Waiting(format!(
        "`{}`: {} connections are not supported yet",
        c.connection.name,
        c.connection.kind_name()
    )))
}

pub async fn put(c: &Resolved, _put: &Put, _public_only: bool) -> Result<(), Failed> {
    Err(Failed::Waiting(format!(
        "`{}`: {} connections are not supported yet",
        c.connection.name,
        c.connection.kind_name()
    )))
}
