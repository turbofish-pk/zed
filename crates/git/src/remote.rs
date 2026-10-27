use std::str::FromStr;
use std::sync::LazyLock;

use derive_more::Deref;
use regex::Regex;
use url::Url;

/// The URL to a Git remote.
#[derive(Debug, PartialEq, Eq, Clone, Deref)]
pub struct RemoteUrl(Url);

// Detect the `user@` prefix of an SCP-like remote (e.g. `git@host:path`). The
// username may contain anything but the `@`/`:`/`/` that delimit the user,
// host, and path, so match by exclusion rather than an allowlist that misses
// names like `first.last`.
static USERNAME_REGEX: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"^[^/@:]+@").expect("Failed to create USERNAME_REGEX"));

impl FromStr for RemoteUrl {
  type Err = url::ParseError;

  fn from_str(input: &str) -> Result<Self, Self::Err> {
    if USERNAME_REGEX.is_match(input) {
      // Rewrite remote URLs like `git@github.com:user/repo.git` to `ssh://git@github.com/user/repo.git`
      let ssh_url = format!("ssh://{}", input.replacen(':', "/", 1));
      Ok(RemoteUrl(Url::parse(&ssh_url)?))
    } else {
      Ok(RemoteUrl(Url::parse(input)?))
    }
  }
}
