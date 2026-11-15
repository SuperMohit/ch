//! [`SessionId`]: the name a session is saved under.

use std::fmt;
use std::str::FromStr;

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// A session id: 1 to [`SessionId::MAX_LEN`] ASCII letters, digits, `-` and `_`.
///
/// An id becomes a file name and the session's directory, which the file tools may
/// read, so it can only be made by [`SessionId::parse`] or [`SessionId::generate`].
/// On disk and in events it is the plain string; a saved file with any other id fails
/// to load.
///
/// [`SessionStore`](crate::SessionStore) takes only a `SessionId`, so an unchecked
/// string can't name a session file:
///
/// ```compile_fail
/// let store = ch_kernel::SessionStore::new("/tmp/sessions");
/// let _ = store.path_for("../elsewhere");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

impl SessionId {
    /// The longest id, well under the 255 bytes most file systems allow in a name.
    pub const MAX_LEN: usize = 128;

    /// `id` as a session id, or an error saying why it can't be one.
    pub fn parse(id: &str) -> anyhow::Result<Self> {
        Self::check(id)?;
        Ok(Self(id.to_string()))
    }

    fn check(id: &str) -> anyhow::Result<()> {
        let valid = !id.is_empty()
            && id.len() <= Self::MAX_LEN
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        anyhow::ensure!(
            valid,
            "`{id}` is not a session id (up to {} letters, digits, `-` and `_`)",
            Self::MAX_LEN
        );
        Ok(())
    }

    /// A new id: the UTC time it was made and a random suffix, as
    /// `20261114-153012-a1b2c3`.
    pub fn generate() -> Self {
        let ts = Utc::now().format("%Y%m%d-%H%M%S");
        let short = uuid::Uuid::new_v4().simple().to_string();
        Self(format!("{ts}-{}", &short[..6]))
    }

    /// The id as text, for messages and comparisons with text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = anyhow::Error;

    fn try_from(id: String) -> anyhow::Result<Self> {
        Self::check(&id)?;
        Ok(Self(id))
    }
}

impl FromStr for SessionId {
    type Err = anyhow::Error;

    fn from_str(id: &str) -> anyhow::Result<Self> {
        Self::parse(id)
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0
    }
}

impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SessionId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for SessionId {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_file_name_safe_ids_parse() {
        for good in ["s1", "20261114-153012-a1b2c3", "a_b-C"] {
            assert_eq!(SessionId::parse(good).unwrap(), good);
        }
        for bad in ["", "..", "../x", "a/b", "a b", "a.jsonl", "é", "a\0"] {
            assert!(SessionId::parse(bad).is_err(), "{bad:?}");
        }
        let longest = "a".repeat(SessionId::MAX_LEN);
        assert!(SessionId::parse(&longest).is_ok());
        assert!(SessionId::parse(&format!("{longest}a")).is_err());
    }

    #[test]
    fn generated_ids_parse_and_differ() {
        let a = SessionId::generate();
        let b = SessionId::generate();
        assert_ne!(a, b);
        assert_eq!(SessionId::parse(a.as_str()).unwrap(), a);
    }

    #[test]
    fn serde_is_the_plain_string_and_validates() {
        let id = SessionId::parse("s1").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"s1\"");
        assert_eq!(serde_json::from_str::<SessionId>("\"s1\"").unwrap(), id);
        assert!(serde_json::from_str::<SessionId>("\"../s1\"").is_err());
    }
}
