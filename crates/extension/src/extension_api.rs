//! Types for the extension registry API on zed.dev.
//!
//! Moved here from the removed `cloud_api_types` crate.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use strum::EnumString;

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct ExtensionApiManifest {
  pub name: String,
  pub version: Arc<str>,
  pub description: Option<String>,
  pub authors: Vec<String>,
  pub repository: String,
  pub schema_version: Option<i32>,
  pub wasm_api_version: Option<String>,
  #[serde(default)]
  pub provides: BTreeSet<ExtensionProvides>,
}

/// What an extension provides.
///
/// The deprecated variants stay, because the registry still sends them for some extensions.
/// Without them, parsing the whole extension list would fail.
#[derive(
  Debug,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  Hash,
  Clone,
  Copy,
  Serialize,
  Deserialize,
  EnumString,
  strum::Display,
  strum::EnumIter,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ExtensionProvides {
  Languages,
  Themes,
  IconThemes,
  Grammars,
  LanguageServers,
  DebugAdapters,
  ContextServers,
  Snippets,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct ExtensionMetadata {
  pub id: Arc<str>,
  #[serde(flatten)]
  pub manifest: ExtensionApiManifest,
  pub published_at: DateTime<Utc>,
  pub download_count: u64,
}

#[derive(Serialize, Deserialize)]
pub struct GetExtensionsResponse {
  pub data: Vec<ExtensionMetadata>,
}
