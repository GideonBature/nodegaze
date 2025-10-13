//! Database repository for event management operations.

use crate::database::models::{
    CreateEvent, Event, EventFilters, EventResponse, EventSeverity, EventType,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

/// Repository for event database operations.
pub struct EventRepository<'a> {
    /// Shared SQLite connection pool
    pool: &'a SqlitePool,
}

impl<'a> EventRepository<'a> {
    /// Creates a new EventRepository instance.
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Creates a new event in the database.
    pub async fn create_event(&self, event: CreateEvent) -> Result<Event> {
        let event = sqlx::query_as!(
            Event,
            r#"
            INSERT INTO events (id, account_id, user_id, node_id, node_alias, event_type, severity, title, description, data, notifications_id, timestamp)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING
            id as "id!",
            account_id as "account_id!",
            user_id as "user_id!",
            node_id as "node_id!",
            node_alias as "node_alias!",
            event_type as "event_type: EventType",
            severity as "severity: EventSeverity",
            title as "title!",
            description as "description!",
            data as "data!",
            notifications_id as "notifications_id!",
            timestamp as "timestamp!: DateTime<Utc>",
            created_at as "created_at!: DateTime<Utc>",
            updated_at as "updated_at!: DateTime<Utc>",
            is_deleted as "is_deleted!",
            deleted_at as "deleted_at?: DateTime<Utc>"
            "#,
            event.id,
            event.account_id,
            event.user_id,
            event.node_id,
            event.node_alias,
            event.event_type,
            event.severity,
            event.title,
            event.description,
            event.data,
            event.notifications_id,
            event.timestamp
        )
        .fetch_one(self.pool)
        .await?;

        Ok(event)
    }

    /// Retrieves events by account ID with basic filtering.
    pub async fn get_events_by_account_id(
        &self,
        account_id: &str,
        filters: Option<EventFilters>,
    ) -> Result<Vec<Event>> {
        let filters = filters.unwrap_or(EventFilters {
            limit: None,
            offset: None,
            node_ids: None,
            event_types: None,
            severities: None,
            start_date: None,
            end_date: None,
        });

        // Simple implementation without complex dynamic queries
        let limit = filters.limit.unwrap_or(50).min(1000);
        let offset = filters.offset.unwrap_or(0);

        let events = sqlx::query_as!(
            Event,
            r#"
            SELECT
            id as "id!",
            account_id as "account_id!",
            user_id as "user_id!",
            node_id as "node_id!",
            node_alias as "node_alias!",
            event_type as "event_type: EventType",
            severity as "severity: EventSeverity",
            title as "title!",
            description as "description!",
            notifications_id as "notifications_id!",
            data as "data!",
            timestamp as "timestamp!: DateTime<Utc>",
            created_at as "created_at!: DateTime<Utc>",
            updated_at as "updated_at!: DateTime<Utc>",
            is_deleted as "is_deleted!",
            deleted_at as "deleted_at?: DateTime<Utc>"
            FROM events
            WHERE account_id = ? AND is_deleted = 0
            ORDER BY timestamp DESC
            LIMIT ? OFFSET ?
            "#,
            account_id,
            limit,
            offset
        )
        .fetch_all(self.pool)
        .await?;

        Ok(events)
    }

    /// Gets events by notification ID.
    pub async fn get_events_by_notification_id(
        &self,
        notifications_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<EventResponse>> {
        let events = sqlx::query_as!(
            Event,
            r#"
              SELECT
              id as "id!",
              account_id as "account_id!",
              user_id as "user_id!",
              node_id as "node_id!",
              node_alias as "node_alias!",
              event_type as "event_type: EventType",
              severity as "severity: EventSeverity",
              title as "title!",
              description as "description!",
              data as "data!",
              timestamp as "timestamp!: DateTime<Utc>",
              notifications_id as "notifications_id?",
              created_at as "created_at!: DateTime<Utc>",
              updated_at as "updated_at!: DateTime<Utc>",
              is_deleted as "is_deleted!",
              deleted_at as "deleted_at?: DateTime<Utc>"
              FROM events
              WHERE notifications_id = ? AND is_deleted = 0
              ORDER BY timestamp DESC
              LIMIT ? OFFSET ?
              "#,
            notifications_id,
            limit,
            offset
        )
        .fetch_all(self.pool)
        .await?;

        // Convert to EventResponse
        let event_responses = events
            .into_iter()
            .map(|event| EventResponse::from(event))
            .collect();

        Ok(event_responses)
    }

    /// Gets event count by notification ID.
    pub async fn count_events_by_notification_id(&self, notifications_id: &str) -> Result<i64> {
        let result = sqlx::query!(
            "SELECT COUNT(*) as count FROM events WHERE notifications_id = ? AND is_deleted = 0",
            notifications_id
        )
        .fetch_one(self.pool)
        .await?;

        Ok(result.count)
    }
}
