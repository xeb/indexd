//! Command ids.
//!
//! Four lowercase hex characters, from a fresh UUID. Short because the id was
//! originally typed into a terminal inside a `[CMD-id]` tag and read back off
//! the screen — a full UUID would have been a line of noise in a pane a human
//! also uses. `indexd` no longer types anything anywhere, but the ids are in
//! the database, in the console's URLs, and in months of logs, so the shape
//! stays.
//!
//! Collisions are possible at four characters and are handled where it
//! matters: `Db::insert` treats a duplicate id as an error rather than an
//! overwrite, because clobbering a live turn's row would lose its reply.

/// A fresh command id.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()[..4].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_four_lowercase_hex_characters() {
        for _ in 0..100 {
            let id = new_id();
            assert_eq!(id.len(), 4, "{id}");
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{id}"
            );
        }
    }
}
