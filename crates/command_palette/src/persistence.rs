use anyhow::Result;
use db::{
  query,
  sqlez::{
    bindable::Column, domain::Domain, statement::Statement,
    thread_safe_connection::ThreadSafeConnection,
  },
  sqlez_macros::sql,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct SerializedCommandUsage {
  pub(crate) command_name: String,
  pub(crate) invocations: u16,
  pub(crate) last_invoked: OffsetDateTime,
}

impl Column for SerializedCommandUsage {
  fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
    let (command_name, next_index): (String, i32) = Column::column(statement, start_index)?;
    let (invocations, next_index): (u16, i32) = Column::column(statement, next_index)?;
    let (last_invoked_raw, next_index): (i64, i32) = Column::column(statement, next_index)?;

    let usage = Self {
      command_name,
      invocations,
      last_invoked: OffsetDateTime::from_unix_timestamp(last_invoked_raw)?,
    };
    Ok((usage, next_index))
  }
}

pub struct CommandPaletteDB(ThreadSafeConnection);

impl Domain for CommandPaletteDB {
  const NAME: &str = stringify!(CommandPaletteDB);
  const MIGRATIONS: &[&str] = &[sql!(
      CREATE TABLE IF NOT EXISTS command_invocations(
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          command_name TEXT NOT NULL,
          user_query TEXT NOT NULL,
          last_invoked INTEGER DEFAULT (unixepoch())  NOT NULL
      ) STRICT;
  )];
}

db::static_connection!(CommandPaletteDB, []);

impl CommandPaletteDB {
  pub async fn write_command_invocation(
    &self,
    command_name: impl Into<String>,
    user_query: impl Into<String>,
  ) -> Result<()> {
    let command_name = command_name.into();
    let user_query = user_query.into();
    log::debug!("Writing command invocation: command_name={command_name}, user_query={user_query}");
    self
      .write_command_invocation_internal(command_name, user_query)
      .await
  }

  query! {
      pub async fn delete_command_history(command_name: String) -> Result<()> {
          DELETE FROM command_invocations WHERE command_name = (?)
      }
  }

  query! {
      pub fn get_command_usage(command: &str) -> Result<Option<SerializedCommandUsage>> {
          SELECT command_name, COUNT(1), MAX(last_invoked)
          FROM command_invocations
          WHERE command_name=(?)
          GROUP BY command_name
      }
  }

  query! {
      async fn write_command_invocation_internal(command_name: String, user_query: String) -> Result<()> {
          INSERT INTO command_invocations (command_name, user_query) VALUES ((?), (?));
          DELETE FROM command_invocations WHERE id IN (SELECT MIN(id) FROM command_invocations HAVING COUNT(1) > 1000);
      }
  }

  query! {
      pub fn list_commands_used() -> Result<Vec<SerializedCommandUsage>> {
          SELECT command_name, COUNT(1), MAX(last_invoked)
          FROM command_invocations
          GROUP BY command_name
          ORDER BY COUNT(1) DESC
      }
  }

  query! {
      pub fn list_recent_queries() -> Result<Vec<String>> {
          SELECT user_query
          FROM command_invocations
          WHERE user_query != ""
          GROUP BY user_query
          ORDER BY MAX(last_invoked) ASC
      }
  }
}
