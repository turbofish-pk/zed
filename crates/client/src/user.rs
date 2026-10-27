//! Users without accounts.
//!
//! This build never signs in, so there is never a current user. The types stay
//! because other crates (project, workspace, title bar) still use them.

use crate::Client;
use gpui::{Context, SharedString, SharedUri};
use postage::watch;
use std::sync::Arc;

pub type UserId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct User {
  pub id: UserId,
  pub github_login: SharedString,
  pub avatar_uri: SharedUri,
  pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectId(pub u64);

impl ProjectId {
  pub fn to_proto(self) -> u64 {
    self.0
  }
}

pub struct UserStore {
  _current_user_tx: watch::Sender<Option<Arc<User>>>,
  current_user_rx: watch::Receiver<Option<Arc<User>>>,
}

impl UserStore {
  pub fn new(_client: Arc<Client>, _cx: &Context<Self>) -> Self {
    let (current_user_tx, current_user_rx) = watch::channel_with(None);
    Self {
      _current_user_tx: current_user_tx,
      current_user_rx,
    }
  }

  /// Always `None`: this build never signs in.
  pub fn current_user(&self) -> Option<Arc<User>> {
    None
  }

  /// Never changes from `None`.
  pub fn watch_current_user(&self) -> watch::Receiver<Option<Arc<User>>> {
    self.current_user_rx.clone()
  }
}