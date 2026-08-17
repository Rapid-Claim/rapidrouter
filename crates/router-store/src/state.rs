//! The replicated state machine: what the log's commands build.

use std::collections::BTreeMap;

use router_core::vkey::VirtualKeyDef;
use serde::{Deserialize, Serialize};

use crate::seal::SealedSecret;

/// Control-plane state. Everything here is small, rarely written, and must
/// converge; ephemeral or high-write state (breakers, usage events) never
/// enters the store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoreState {
    /// The managed config document (TOML). `None` until first seeded.
    #[serde(default)]
    pub config_text: Option<String>,
    /// Virtual keys by id (hash form only — never secret material).
    #[serde(default)]
    pub virtual_keys: BTreeMap<String, VirtualKeyDef>,
    /// Sealed `store.*` secrets by name (ciphertext; replicates safely).
    #[serde(default)]
    pub secrets: BTreeMap<String, SealedSecret>,
    /// Console/admin settings (retention, appearance, …).
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// Console users (password hashes only, never passwords).
    #[serde(default)]
    pub users: BTreeMap<String, router_core::access::UserDef>,
    /// Teams: membership, model scope, and access level.
    #[serde(default)]
    pub teams: BTreeMap<String, router_core::access::TeamDef>,
}

/// A log entry. Applying is infallible and deterministic — all validation
/// (config parses, key ids well-formed) happens before a command is
/// proposed, so every replica applies identically.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    PutConfig { text: String },
    PutVirtualKey { def: VirtualKeyDef },
    DeleteVirtualKey { id: String },
    PutSecret { name: String, sealed: SealedSecret },
    DeleteSecret { name: String },
    PutSetting { name: String, value: String },
    DeleteSetting { name: String },
    PutUser { def: router_core::access::UserDef },
    DeleteUser { id: String },
    PutTeam { def: router_core::access::TeamDef },
    DeleteTeam { id: String },
}

impl StoreState {
    pub fn apply(&mut self, command: &Command) {
        match command {
            Command::PutConfig { text } => self.config_text = Some(text.clone()),
            Command::PutVirtualKey { def } => {
                self.virtual_keys.insert(def.id.clone(), def.clone());
            }
            Command::DeleteVirtualKey { id } => {
                self.virtual_keys.remove(id);
            }
            Command::PutSecret { name, sealed } => {
                self.secrets.insert(name.clone(), sealed.clone());
            }
            Command::DeleteSecret { name } => {
                self.secrets.remove(name);
            }
            Command::PutSetting { name, value } => {
                self.settings.insert(name.clone(), value.clone());
            }
            Command::DeleteSetting { name } => {
                self.settings.remove(name);
            }
            Command::PutUser { def } => {
                self.users.insert(def.id.clone(), def.clone());
            }
            Command::DeleteUser { id } => {
                self.users.remove(id);
                // Membership references the id; a deleted user must not
                // linger as a ghost member that a rename could resurrect.
                for team in self.teams.values_mut() {
                    team.members.remove(id);
                }
            }
            Command::PutTeam { def } => {
                self.teams.insert(def.id.clone(), def.clone());
            }
            Command::DeleteTeam { id } => {
                self.teams.remove(id);
            }
        }
    }

    pub fn virtual_key_defs(&self) -> Vec<VirtualKeyDef> {
        self.virtual_keys.values().cloned().collect()
    }
}
