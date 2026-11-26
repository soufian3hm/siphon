//! Passwords, kept out of the settings file.
//!
//! Site logins are stored in the operating system's credential store (the
//! macOS Keychain, Windows Credential Manager, or the Secret Service on
//! Linux). Tests and scratch runs use an in-memory store instead, so they
//! never touch the user's keychain.

use std::collections::HashMap;

use parking_lot::Mutex;

const SERVICE: &str = "Siphon";

enum Backend {
    System,
    Memory(Mutex<HashMap<String, String>>),
}

pub struct Secrets {
    backend: Backend,
    /// What has been read this session. A keychain read can put a system
    /// prompt in front of the user; once is enough.
    cache: Mutex<HashMap<String, Option<String>>>,
}

impl Secrets {
    /// The operating system's credential store.
    pub fn system() -> Self {
        Self { backend: Backend::System, cache: Mutex::new(HashMap::new()) }
    }

    /// A store that lives and dies with the process.
    pub fn in_memory() -> Self {
        Self { backend: Backend::Memory(Mutex::new(HashMap::new())), cache: Mutex::new(HashMap::new()) }
    }

    pub fn is_system(&self) -> bool {
        matches!(self.backend, Backend::System)
    }

    /// The password stored under `account`, if there is one. May block on a
    /// system prompt; call it off the interface thread.
    pub fn get(&self, account: &str) -> Result<Option<String>, String> {
        if let Some(known) = self.cache.lock().get(account) {
            return Ok(known.clone());
        }
        let found = match &self.backend {
            Backend::Memory(map) => map.lock().get(account).cloned(),
            Backend::System => match keyring::Entry::new(SERVICE, account).and_then(|entry| entry.get_password()) {
                Ok(password) => Some(password),
                Err(keyring::Error::NoEntry) => None,
                Err(e) => return Err(e.to_string()),
            },
        };
        self.cache.lock().insert(account.to_owned(), found.clone());
        Ok(found)
    }

    pub fn set(&self, account: &str, password: &str) -> Result<(), String> {
        match &self.backend {
            Backend::Memory(map) => {
                map.lock().insert(account.to_owned(), password.to_owned());
            }
            Backend::System => keyring::Entry::new(SERVICE, account).and_then(|entry| entry.set_password(password)).map_err(|e| e.to_string())?,
        }
        self.cache.lock().insert(account.to_owned(), Some(password.to_owned()));
        Ok(())
    }

    pub fn delete(&self, account: &str) -> Result<(), String> {
        match &self.backend {
            Backend::Memory(map) => {
                map.lock().remove(account);
            }
            Backend::System => match keyring::Entry::new(SERVICE, account).and_then(|entry| entry.delete_credential()) {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => return Err(e.to_string()),
            },
        }
        self.cache.lock().insert(account.to_owned(), None);
        Ok(())
    }
}

/// The keychain account a site rule's password is stored under.
pub fn site_account(pattern: &str, username: &str) -> String {
    format!("{}@{}", username.trim(), pattern.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_round_trips() {
        let secrets = Secrets::in_memory();
        assert!(!secrets.is_system());
        assert_eq!(secrets.get("ada@files.example"), Ok(None));
        secrets.set("ada@files.example", "correct horse").unwrap();
        assert_eq!(secrets.get("ada@files.example"), Ok(Some("correct horse".into())));
        secrets.set("ada@files.example", "battery staple").unwrap();
        assert_eq!(secrets.get("ada@files.example"), Ok(Some("battery staple".into())));
        secrets.delete("ada@files.example").unwrap();
        assert_eq!(secrets.get("ada@files.example"), Ok(None));
        secrets.delete("never-existed").unwrap();
    }

    /// Writes to the real credential store, so it only runs when asked for:
    /// `cargo test -p siphon-core --lib secrets -- --ignored`.
    #[test]
    #[ignore = "touches the operating system's credential store"]
    fn the_system_store_round_trips() {
        let account = format!("selftest-{}@siphon.invalid", std::process::id());
        let secrets = Secrets::system();
        assert!(secrets.is_system());
        assert_eq!(secrets.get(&account), Ok(None));
        secrets.set(&account, "p\u{00E4}ss w\u{00F6}rd").unwrap();
        // A second handle has no cache: this read really comes from the store.
        assert_eq!(Secrets::system().get(&account), Ok(Some("p\u{00E4}ss w\u{00F6}rd".into())));
        secrets.delete(&account).unwrap();
        assert_eq!(Secrets::system().get(&account), Ok(None));
    }

    #[test]
    fn accounts_are_per_user_and_site() {
        assert_eq!(site_account(" Files.Example.com ", " ada "), "ada@files.example.com");
        assert_ne!(site_account("a.example", "ada"), site_account("a.example", "bob"));
    }
}
