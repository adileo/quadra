//! (to be written)

use super::{Ask, Failed, Fetched, Resolved};

pub async fn fetch(c: &Resolved, _ask: &Ask, _public_only: bool) -> Result<Fetched, Failed> {
    Err(Failed::Waiting(format!(
        "`{}`: {} connections are not supported yet",
        c.connection.name,
        c.connection.kind_name()
    )))
}
