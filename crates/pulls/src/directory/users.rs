//! People and their handles: the upsert at sign-in, handle reservation and release, lookup by
//! handle or email.

use super::*;

impl Directory {
    /// Record that this person exists and has just been seen. Called on every
    /// sign-in, so it must be an upsert: the first one creates the row, the rest
    /// only move `lastSeenAt` and refresh the display name.
    pub async fn upsert_user(&self, email: &str, name: &str) -> Result<User> {
        let email = email.trim().to_lowercase();
        if !email.contains('@') {
            return Err(invalid("a valid email is required"));
        }
        let name = if name.trim().is_empty() { email.split('@').next().unwrap_or(&email) } else { name.trim() };
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => {
                m.users
                    .update_one(
                        doc! { "_id": &email },
                        doc! {
                            "$set": { "name": name, "lastSeenAt": now },
                            // Only on insert: a returning user keeps the date they joined.
                            "$setOnInsert": { "createdAt": now },
                        },
                    )
                    .upsert(true)
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                match s.users.get_mut(&email) {
                    Some(u) => {
                        u.name = name.to_string();
                        u.last_seen_at = now;
                    }
                    None => {
                        s.users.insert(
                            email.clone(),
                            User {
                                email: email.clone(),
                                name: name.to_string(),
                                username: None,
                                created_at: now,
                                last_seen_at: now,
                            },
                        );
                    }
                }
            }
        }
        self.user(&email)
            .await?
            .ok_or_else(|| err("user vanished immediately after upsert"))
    }

    /// Reserve `handle` for `kind`, held by `held_by`. `Ok(false)` means it is
    /// already taken — by a user or a team, which is the point of one collection.
    pub(crate) async fn reserve(&self, handle: &str, kind: HandleKind, held_by: &str) -> Result<bool> {
        let doc = Handle {
            handle: handle.to_string(),
            kind,
            held_by: held_by.to_string(),
            created_at: DateTime::now(),
        };
        match &self.backend {
            Backend::Mongo(m) => match m.handles.insert_one(&doc).await {
                Ok(_) => Ok(true),
                Err(e) if is_duplicate_key(&e) => Ok(false),
                Err(e) => Err(err(format!("mongo: {e}"))),
            },
            Backend::Memory(s) => {
                // `_id` uniqueness, which is what makes the insert the gate.
                let mut s = s.lock().unwrap();
                if s.handles.contains_key(handle) {
                    return Ok(false);
                }
                s.handles.insert(handle.to_string(), doc);
                Ok(true)
            }
        }
    }

    pub(crate) async fn release(&self, handle: &str) -> Result<()> {
        match &self.backend {
            Backend::Mongo(m) => m
                .handles
                .delete_one(doc! { "_id": handle })
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                s.lock().unwrap().handles.remove(handle);
                Ok(())
            }
        }
    }

    /// Claim a username. `Ok(None)` means the handle is taken.
    ///
    /// Reserving comes first and is the gate: two people racing for one handle
    /// both reach the insert, and exactly one wins. Only then is it written to the
    /// user. The write itself is conditional on the username still being absent, so
    /// two claims by one person cannot both land; the loser gives its reservation back.
    pub async fn claim_username(&self, email: &str, handle: &str) -> Result<Option<User>> {
        let email = email.trim().to_lowercase();
        let handle = handle.trim().to_lowercase();
        check_handle(&handle)?;

        let existing = self.user(&email).await?.ok_or_else(|| invalid("no such user"))?;
        if let Some(current) = &existing.username {
            // Not an error: asking again for the handle you already hold is a
            // retry, and should look like it worked.
            return if *current == handle { Ok(Some(existing)) } else { Err(invalid("username already set")) };
        }
        if !self.reserve(&handle, HandleKind::User, &email).await? {
            return Ok(None);
        }
        // Conditional on the handle still being unset: two claims for one user can both pass the
        // read above, and an unconditional `$set` would let the second overwrite the first, whose
        // reservation is then held by nobody forever. Zero matched means somebody won first.
        let set = match &self.backend {
            Backend::Mongo(m) => m
                .users
                .update_one(
                    doc! { "_id": &email, "username": { "$exists": false } },
                    doc! { "$set": { "username": &handle } },
                )
                .await
                .map(|r| r.matched_count == 1),
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                Ok(match s.users.get_mut(&email) {
                    Some(u) if u.username.is_none() => {
                        u.username = Some(handle.clone());
                        true
                    }
                    _ => false,
                })
            }
        };
        match set {
            Ok(true) => self.user(&email).await,
            Ok(_) => {
                let _ = self.release(&handle).await;
                Err(invalid("username already set"))
            }
            Err(e) => {
                // Compensate, or the handle is reserved for a user who does not
                // carry it — unclaimable by anyone, forever.
                let _ = self.release(&handle).await;
                Err(err(format!("mongo: {e}")))
            }
        }
    }

    /// The person behind a handle — what a workspace needs to sign commits as them.
    pub async fn user_by_handle(&self, handle: &str) -> Result<Option<User>> {
        let handle = handle.trim().to_lowercase();
        match &self.backend {
            Backend::Mongo(m) => m
                .users
                .find_one(doc! { "username": handle })
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                Ok(s.lock().unwrap().users.values().find(|u| u.username.as_deref() == Some(&handle)).cloned())
            }
        }
    }

    pub async fn user(&self, email: &str) -> Result<Option<User>> {
        let email = email.trim().to_lowercase();
        match &self.backend {
            Backend::Mongo(m) => m
                .users
                .find_one(doc! { "_id": email })
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s.lock().unwrap().users.get(&email).cloned()),
        }
    }


    // ── credentials ─────────────────────────────────────────────────────────
}
