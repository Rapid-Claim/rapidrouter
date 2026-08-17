//! Internal users and teams: who may sign in to the console, and what a
//! signed-in person is allowed to touch.
//!
//! This is console/control-plane identity only. Data-plane callers keep
//! authenticating with virtual keys — a user here is a person at the
//! console, not an application at `/v1`.

use std::collections::BTreeSet;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use serde::{Deserialize, Serialize};

/// A person allowed to sign in to the console.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserDef {
    pub id: String,
    pub email: String,
    /// Argon2id in PHC string form. Passwords are low-entropy secrets, so
    /// they get a memory-hard KDF — unlike virtual-key secrets, which are
    /// 256-bit random values and safely stored as a fast hash.
    pub password_hash: String,
    pub role: UserRole,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    /// Everything, including managing users and teams.
    Admin,
    /// Whatever their teams grant, and nothing by default.
    Member,
}

/// A team: members, the models those members may use, and how much of the
/// console they may operate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamDef {
    pub id: String,
    pub name: String,
    /// User ids.
    #[serde(default)]
    pub members: BTreeSet<String>,
    /// Model targets members may route to (`provider/model` or a routing
    /// group name). Empty means every model — an *unrestricted* team, not
    /// a useless one; restriction is opt-in.
    #[serde(default)]
    pub models: BTreeSet<String>,
    pub access: TeamAccess,
    pub created_ms: u64,
}

/// Ordered weakest→strongest so a member of several teams gets the union
/// of what they grant via `max`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TeamAccess {
    /// See everything, change nothing.
    ReadOnly,
    /// Create and manage virtual keys within the team's models.
    Keys,
    /// Operate the whole console, like an admin without user management.
    Full,
}

/// What one signed-in principal may do, resolved from their role and
/// every team they belong to.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub access: TeamAccess,
    /// `None` = every model; `Some` = only these targets.
    pub models: Option<BTreeSet<String>>,
    /// Team ids, for tagging the keys a scoped member creates.
    pub teams: Vec<String>,
}

impl Grant {
    pub fn admin() -> Self {
        Self {
            access: TeamAccess::Full,
            models: None,
            teams: Vec::new(),
        }
    }

    /// Resolve a member's grant from their teams.
    ///
    /// Access is the strongest any team gives. Models are the union — and
    /// one unrestricted team unrestricts the union, because "everything"
    /// absorbs any list. A member of no teams can read and nothing else:
    /// existence of an account is not a grant.
    pub fn for_member<'a>(user_id: &str, teams: impl Iterator<Item = &'a TeamDef>) -> Self {
        let mut access = TeamAccess::ReadOnly;
        let mut models: Option<BTreeSet<String>> = Some(BTreeSet::new());
        let mut member_of = Vec::new();
        for team in teams {
            if !team.members.contains(user_id) {
                continue;
            }
            member_of.push(team.id.clone());
            access = access.max(team.access);
            match (&mut models, team.models.is_empty()) {
                (None, _) => {}
                (slot @ Some(_), true) => *slot = None,
                (Some(set), false) => set.extend(team.models.iter().cloned()),
            }
        }
        if member_of.is_empty() {
            // No team granted a model list either; an empty Some would
            // read as "no models", which is correct for the teamless.
            models = Some(BTreeSet::new());
        }
        Self {
            access,
            models,
            teams: member_of,
        }
    }

    /// Whether every requested model target is inside this grant.
    pub fn allows_models<'a>(&self, requested: impl IntoIterator<Item = &'a str>) -> bool {
        match &self.models {
            None => true,
            Some(allowed) => requested.into_iter().all(|m| allowed.contains(m)),
        }
    }
}

/// Hash a password for storage. Argon2id, default parameters, random salt.
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| err.to_string())
}

/// Verify a presented password against a stored PHC string.
///
/// A malformed stored hash verifies as false rather than erroring: the
/// caller is an authentication path, and the only safe answer to "this
/// record is corrupt" is "you are not signed in".
pub fn verify_password(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored)
        .map(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
        .unwrap_or(false)
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn team(id: &str, members: &[&str], models: &[&str], access: TeamAccess) -> TeamDef {
        TeamDef {
            id: id.into(),
            name: id.into(),
            members: members.iter().map(|m| m.to_string()).collect(),
            models: models.iter().map(|m| m.to_string()).collect(),
            access,
            created_ms: 0,
        }
    }

    #[test]
    fn a_password_round_trips_and_a_wrong_one_fails() {
        let hash = hash_password("hunter2!").unwrap();
        assert!(verify_password("hunter2!", &hash));
        assert!(!verify_password("hunter3!", &hash));
        assert!(!verify_password("hunter2!", "not-a-phc-string"));
    }

    #[test]
    fn a_teamless_member_can_read_and_nothing_else() {
        let grant = Grant::for_member("u1", [].iter());
        assert_eq!(grant.access, TeamAccess::ReadOnly);
        assert!(!grant.allows_models(["openai/gpt-4o"]));
    }

    #[test]
    fn access_is_the_strongest_of_any_team() {
        let teams = [
            team("a", &["u1"], &["openai/gpt-4o"], TeamAccess::ReadOnly),
            team("b", &["u1"], &["groq/llama"], TeamAccess::Keys),
        ];
        let grant = Grant::for_member("u1", teams.iter());
        assert_eq!(grant.access, TeamAccess::Keys);
        assert!(grant.allows_models(["openai/gpt-4o", "groq/llama"]));
        assert!(!grant.allows_models(["anthropic/claude-haiku-4-5"]));
    }

    #[test]
    fn one_unrestricted_team_unrestricts_the_union() {
        let teams = [
            team("a", &["u1"], &["openai/gpt-4o"], TeamAccess::Keys),
            team("b", &["u1"], &[], TeamAccess::ReadOnly),
        ];
        let grant = Grant::for_member("u1", teams.iter());
        assert!(grant.models.is_none(), "empty list means every model");
        assert!(grant.allows_models(["anything/at-all"]));
    }

    #[test]
    fn membership_is_checked_not_assumed() {
        let teams = [team("a", &["someone-else"], &[], TeamAccess::Full)];
        let grant = Grant::for_member("u1", teams.iter());
        assert_eq!(grant.access, TeamAccess::ReadOnly);
    }
}
