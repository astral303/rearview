//! The agent's semantic route text of each session's copy, stored by the
//! first semantic search that computes it from the copy's texts.

use super::base::CacheBase;
use super::coordinator::WaitStatus;
use super::scope::{OperationAccess, OperationScope, Patience};
use super::{CacheWriter, SqliteCache, refresh};
use crate::error::Result;
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
/// `session` with `custom_title` and `summary`; `None` when none is.
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
             AND custom_title IS ?3 AND summary IS ?4",
        )?
        .query_row(params![session, generation, custom_title, summary], |row| {
            row.get(0)
        })
        .optional()?)
}

/// What [`store_routes`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RoutesStored {
    Stored,
    /// Another refresh owns the cache: nothing was written.
    NotOwner,
}

/// Store each of `routes` whose copy the cache under `base` still holds, as
/// the refresh owner. Takes refresh ownership without waiting, and writes
/// nothing while another refresh owns the cache.
pub(crate) fn store_routes(base: &CacheBase, routes: &[SemanticRoute]) -> Result<RoutesStored> {
    let operation = OperationScope::new(
        base,
        Patience::AtMost(Duration::ZERO),
        Arc::new(WaitStatus::default()),
    );
    let stored = match operation.acquire_access()? {
        OperationAccess::Owner(owner) => {
            SqliteCache::open_for_refresh(owner.token())?.store_semantic_routes(routes)?;
            RoutesStored::Stored
        }
        OperationAccess::AsIs => RoutesStored::NotOwner,
    };
    operation.finish()?;
    Ok(stored)
}

impl CacheWriter {
    /// Store each of `routes` whose copy the cache still holds, in one batch.
    pub(super) fn store_semantic_routes(&self, routes: &[SemanticRoute]) -> Result<()> {
        let batch = refresh::transaction(&self.connection, &self.owner)?;
        {
            let mut statement = batch.prepare_cached(
                "INSERT OR REPLACE INTO semantic_routes \
                 (session, generation, custom_title, summary, text) \
                 SELECT ?1, ?2, ?3, ?4, ?5 FROM sessions WHERE id = ?1 AND generation = ?2",
            )?;
            for route in routes {
                statement.execute(params![
                    route.key.session,
                    route.key.generation,
                    route.custom_title,
                    route.summary,
                    route.text
                ])?;
            }
        }
        refresh::commit(batch, &self.owner)?;
        Ok(())
    }
}
