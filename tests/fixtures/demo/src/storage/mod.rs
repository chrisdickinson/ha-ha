//! Storage boundary fixture.

/// A user record.
pub struct User {
    pub id: u64,
    pub name: String,
}

/// The storage boundary the domain layer consumes.
pub trait Repository {
    /// Find a single user by id.
    fn find_user(&self, id: u64) -> Option<User>;

    /// List every user.
    fn list_users(&self) -> Vec<User>;
}

/// Not part of the boundary — a private helper.
fn open_pool(url: &str, max: usize) -> String {
    format!("{url}:{max}")
}

impl User {
    /// Build a user.
    pub fn new(id: u64, name: String) -> Self {
        Self { id, name }
    }
}

/// A type alias.
pub type UserId = u64;

/// An enum, for kind coverage.
pub enum Backend {
    Sqlite,
    Postgres { dsn: String },
}
