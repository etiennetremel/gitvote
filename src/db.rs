use crate::{
    cfg::CfgProfile,
    cmd::CreateVoteInput,
    github::{split_full_name, DynGH},
    results::{self, Vote, VoteResults},
};
use anyhow::Result;
use async_trait::async_trait;
use deadpool_postgres::{Pool, Transaction};
#[cfg(test)]
use mockall::automock;
use std::sync::Arc;
use tokio_postgres::types::Json;
use uuid::Uuid;

/// Type alias to represent a DB trait object.
pub(crate) type DynDB = Arc<dyn DB + Send + Sync>;

/// Trait that defines some operations a DB implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait DB {
    /// Cancel open vote (if exists) in the issue/pr provided.
    async fn cancel_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
    ) -> Result<Option<Uuid>>;

    /// Close any pending finished vote.
    async fn close_finished_vote(&self, gh: DynGH) -> Result<Option<(Vote, VoteResults)>>;

    /// Check if the issue/pr provided has a vote.
    async fn has_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<bool>;

    /// Check if the issue/pr provided already has a vote open.
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool>;

    /// Store the vote provided in the database.
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        input: &CreateVoteInput,
        cfg: &CfgProfile,
    ) -> Result<Uuid>;
}

/// DB implementation backed by PostgreSQL.
pub(crate) struct PgDB {
    pool: Pool,
}

impl PgDB {
    /// Create a new PgDB instance.
    pub(crate) fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Get any pending finished vote.
    async fn get_pending_finished_vote(tx: &Transaction<'_>) -> Result<Option<Vote>> {
        let vote = tx
            .query_opt(
                "
                select
                    vote_id,
                    vote_comment_id,
                    created_at,
                    created_by,
                    ends_at,
                    closed,
                    closed_at,
                    cfg,
                    installation_id,
                    issue_id,
                    issue_number,
                    is_pull_request,
                    repository_full_name,
                    organization,
                    results
                from vote
                where current_timestamp > ends_at and closed = false
                for update of vote skip locked
                limit 1
                ",
                &[],
            )
            .await?
            .map(|row| {
                let Json(cfg): Json<CfgProfile> = row.get("cfg");
                let results: Option<Json<VoteResults>> = row.get("results");
                Vote {
                    vote_id: row.get("vote_id"),
                    vote_comment_id: row.get("vote_comment_id"),
                    created_at: row.get("created_at"),
                    created_by: row.get("created_by"),
                    ends_at: row.get("ends_at"),
                    closed: row.get("closed"),
                    closed_at: row.get("closed_at"),
                    cfg,
                    installation_id: row.get("installation_id"),
                    issue_id: row.get("issue_id"),
                    issue_number: row.get("issue_number"),
                    is_pull_request: row.get("is_pull_request"),
                    repository_full_name: row.get("repository_full_name"),
                    organization: row.get("organization"),
                    results: results.map(|Json(results)| results),
                }
            });
        Ok(vote)
    }

    /// Store the vote results provided in the database.
    async fn store_vote_results(
        tx: &Transaction<'_>,
        vote_id: Uuid,
        results: &VoteResults,
    ) -> Result<()> {
        tx.execute(
            "
            update vote set
                closed = true,
                closed_at = current_timestamp,
                results = $1::jsonb
            where vote_id = $2::uuid;
            ",
            &[&Json(&results), &vote_id],
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl DB for PgDB {
    async fn cancel_vote(
        &self,
        repository_full_name: &str,
        issue_number: i64,
    ) -> Result<Option<Uuid>> {
        let db = self.pool.get().await?;
        let cancelled_vote_id = db
            .query_opt(
                "
                delete from vote
                where repository_full_name = $1::text
                and issue_number = $2::bigint
                and closed = false
                returning vote_id
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .and_then(|row| row.get("vote_id"));
        Ok(cancelled_vote_id)
    }

    async fn close_finished_vote(&self, gh: DynGH) -> Result<Option<(Vote, VoteResults)>> {
        // Get pending finished vote (if any) from database
        let mut db = self.pool.get().await?;
        let tx = db.transaction().await?;
        let vote = match PgDB::get_pending_finished_vote(&tx).await? {
            Some(vote) => vote,
            None => return Ok(None),
        };

        // Calculate results
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let results = results::calculate(gh, owner, repo, &vote).await?;

        // Store results in database
        PgDB::store_vote_results(&tx, vote.vote_id, &results).await?;
        tx.commit().await?;

        Ok(Some((vote, results)))
    }

    async fn has_vote(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        let db = self.pool.get().await?;
        let has_vote = db
            .query_one(
                "
                select exists (
                    select 1 from vote
                    where repository_full_name = $1::text
                    and issue_number = $2::bigint
                )
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .get(0);
        Ok(has_vote)
    }

    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        let db = self.pool.get().await?;
        let has_vote_open = db
            .query_one(
                "
                select exists (
                    select 1 from vote
                    where repository_full_name = $1::text
                    and issue_number = $2::bigint
                    and closed = false
                )
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?
            .get(0);
        Ok(has_vote_open)
    }

    async fn store_vote(
        &self,
        vote_comment_id: i64,
        input: &CreateVoteInput,
        cfg: &CfgProfile,
    ) -> Result<Uuid> {
        let db = self.pool.get().await?;
        let vote_id = db
            .query_one(
                "
                insert into vote (
                    vote_comment_id,
                    ends_at,
                    cfg,
                    created_by,
                    installation_id,
                    issue_id,
                    issue_number,
                    is_pull_request,
                    repository_full_name,
                    organization

                ) values (
                    $1::bigint,
                    current_timestamp + ($2::bigint || ' seconds')::interval,
                    $3::jsonb,
                    $4::text,
                    $5::bigint,
                    $6::bigint,
                    $7::bigint,
                    $8::boolean,
                    $9::text,
                    $10::text
                )
                returning vote_id
                ",
                &[
                    &vote_comment_id,
                    &(cfg.duration.as_secs() as i64),
                    &Json(&cfg),
                    &input.created_by,
                    &input.installation_id,
                    &input.issue_id,
                    &input.issue_number,
                    &input.is_pull_request,
                    &input.repository_full_name,
                    &input.organization,
                ],
            )
            .await?
            .get("vote_id");
        Ok(vote_id)
    }
}
