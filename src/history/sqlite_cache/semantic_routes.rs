//! The agent's semantic route text of each session's copy, stored by a
//! semantic search or semantic cache generation once it computes it from the
//! copy's texts.

use super::base::CacheBase;
use super::coordinator::WaitStatus;
use super::scope::{OperationAccess, OperationScope, Patience};
use super::{CacheWriter, SqliteCache, refresh};
use crate::error::Result;
use crate::history::SEMANTIC_ROUTE_VERSION;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Arc;
use std::time::Duration;

/// The copy a route text is stored with: its session row and generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RouteKey {
    session: i64,
    generation: i64,
}

impl RouteKey {
    pub(super) fn new(session: i64, generation: i64) -> Self {
        Self {
            session,
            generation,
        }
    }

    /// `text`, the route text computed from the copy's texts with
    /// `custom_title` and `summary`, to store with the copy.
    pub(crate) fn route(
        self,
        custom_title: Option<&str>,
        summary: Option<&str>,
        text: String,
    ) -> SemanticRoute {
        SemanticRoute {
            key: self,
            custom_title: custom_title.map(str::to_owned),
            summary: summary.map(str::to_owned),
            text,
        }
    }
}

/// A copy's route text, computed from its texts and the row's title and
/// summary, to store with the copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SemanticRoute {
    key: RouteKey,
    custom_title: Option<String>,
    summary: Option<String>,
    text: String,
}

/// The route text stored for the copy `generation` of the session row
/// `session` with `custom_title` and `summary`, by this build's route
/// algorithm; `None` when none is.
pub(super) fn stored_route(
    connection: &Connection,
    session: i64,
    generation: i64,
    custom_title: Option<&str>,
    summary: Option<&str>,
) -> Result<Option<String>> {
    Ok(connection
        .prepare_cached(
            "SELECT text FROM semantic_routes WHERE session = ?1 AND generation = ?2 \
             AND custom_title IS ?3 AND summary IS ?4 AND version = ?5",
        )?
        .query_row(
            params![
                session,
                generation,
                custom_title,
                summary,
                SEMANTIC_ROUTE_VERSION
            ],
            |row| row.get(0),
        )
        .optional()?)
}

/// The outcome of [`store_routes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RoutesStored {
    Stored,
    /// Another refresh owns the cache: nothing was written.
    NotOwner,
}

/// Store each of `routes` whose copy the cache under `base` still holds, as
/// the refresh owner. Takes refresh ownership without waiting, and writes
/// nothing while another refresh owns the cache. When the drive is low on
/// space, fails with [`OperationScope::finish`]'s error, in place of any
/// error of the write itself.
pub(crate) fn store_routes(base: &CacheBase, routes: &[SemanticRoute]) -> Result<RoutesStored> {
    let operation = OperationScope::new(
        base,
        Patience::AtMost(Duration::ZERO),
        Arc::new(WaitStatus::default()),
    );
    let stored = store_as_owner(&operation, routes);
    operation.finish()?;
    stored
}

fn store_as_owner(operation: &OperationScope, routes: &[SemanticRoute]) -> Result<RoutesStored> {
    Ok(match operation.acquire_access()? {
        OperationAccess::Owner(owner) => {
            SqliteCache::open_for_refresh(owner.token())?.store_semantic_routes(routes)?;
            RoutesStored::Stored
        }
        OperationAccess::AsIs => RoutesStored::NotOwner,
    })
}

impl CacheWriter {
    /// Store each of `routes` whose copy the cache still holds, in one batch.
    /// Fails without writing when the drive is low on space.
    pub(super) fn store_semantic_routes(&self, routes: &[SemanticRoute]) -> Result<()> {
        self.owner.check_space()?;
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        {
            let mut statement = batch.prepare_cached(
                "INSERT OR REPLACE INTO semantic_routes \
                 (session, generation, custom_title, summary, text, version) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6 FROM sessions WHERE id = ?1 AND generation = ?2",
            )?;
            for route in routes {
                statement.execute(params![
                    route.key.session,
                    route.key.generation,
                    route.custom_title,
                    route.summary,
                    route.text,
                    SEMANTIC_ROUTE_VERSION
                ])?;
            }
        }
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }
}
