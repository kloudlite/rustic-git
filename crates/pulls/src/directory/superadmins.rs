//! The `superadmins` collection: the claim minted into a session JWT, bootstrapped from
//! `KLOUDLITE_WORKSPACES_ADMINS` (additive) and managed through the admin API after that.

use super::*;

impl Directory {
    pub async fn is_superadmin(&self, user: &str) -> Result<bool> {
        let user = user.trim().to_lowercase();
        match &self.backend {
            Backend::Mongo(m) => Ok(m
                .superadmins
                .find_one(doc! { "_id": user })
                .await
                .map_err(|e| err(format!("mongo: {e}")))?
                .is_some()),
            Backend::Memory(s) => Ok(s.lock().unwrap().superadmins.contains_key(&user)),
        }
    }

    pub async fn superadmins(&self) -> Result<Vec<SuperAdmin>> {
        use futures::TryStreamExt;
        match &self.backend {
            Backend::Mongo(m) => m
                .superadmins
                .find(doc! {})
                .limit(super::LISTING_LIMIT)
                .max_time(super::QUERY_MAX_TIME)
                .await
                .map_err(|e| err(format!("mongo: {e}")))?
                .try_collect()
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s.lock().unwrap().superadmins.values().cloned().collect()),
        }
    }

    /// Idempotent: granting twice is not an error, and it must not rewrite who granted it first.
    pub async fn add_superadmin(&self, user: &str, by: &str) -> Result<()> {
        let user = user.trim().to_lowercase();
        let row = SuperAdmin { user: user.clone(), added_at: DateTime::now(), added_by: by.to_string() };
        match &self.backend {
            Backend::Mongo(m) => {
                m.superadmins
                    .update_one(
                        doc! { "_id": &user },
                        doc! { "$setOnInsert": mongodb::bson::to_document(&row).map_err(|e| err(format!("bson: {e}")))? },
                    )
                    .upsert(true)
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
            }
            // `$setOnInsert`: an existing row keeps the `addedBy` it was granted with.
            Backend::Memory(s) => {
                s.lock().unwrap().superadmins.entry(user).or_insert(row);
            }
        }
        Ok(())
    }

    /// Revoke, refusing to empty the roster. `Ok(false)` means the row is still there because
    /// removing it would have left nobody.
    ///
    /// The caller used to list the roster, judge "not the last one", and then delete — two round
    /// trips with a window between them, so two concurrent removes each saw two administrators
    /// and each deleted one, leaving a cluster nobody could administer (2026-09-12). This is the
    /// compare-and-swap the check always needed: the delete happens, the roster is counted, and a
    /// delete that emptied it puts the row back EXACTLY as it was — `added_at` and `added_by`
    /// included, which is why the deleted document is what gets re-inserted rather than a fresh
    /// one. Whichever caller loses the race restores and reports `false`.
    pub async fn remove_superadmin(&self, user: &str) -> Result<bool> {
        let user = user.trim().to_lowercase();
        match &self.backend {
            Backend::Mongo(m) => {
                let Some(row) = m
                    .superadmins
                    .find_one_and_delete(doc! { "_id": &user })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?
                else {
                    return Ok(true); // already gone: the end state the caller asked for
                };
                let left = m
                    .superadmins
                    .count_documents(doc! {})
                    .max_time(super::QUERY_MAX_TIME)
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                if left == 0 {
                    m.superadmins
                        .insert_one(&row)
                        .await
                        .map_err(|e| err(format!("mongo: restoring the last administrator: {e}")))?;
                    return Ok(false);
                }
            }
            Backend::Memory(s) => {
                let mut g = s.lock().unwrap();
                let Some(row) = g.superadmins.remove(&user) else { return Ok(true) };
                if g.superadmins.is_empty() {
                    g.superadmins.insert(user, row);
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// The `KLOUDLITE_WORKSPACES_ADMINS` bootstrap, run once at boot. It only ever ADDS: the env
    /// is a way to get the first administrator into an empty cluster, not the list itself, so
    /// removing an email from it must not silently revoke someone the list has since granted.
    pub async fn ensure_superadmins(&self, emails: &[String]) -> Result<usize> {
        let mut n = 0;
        for e in emails {
            if !self.is_superadmin(e).await? {
                self.add_superadmin(e, "bootstrap").await?;
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The roster can never be emptied, whatever order the removes arrive in. The api's own
    /// "is this the last one" read happens a round trip earlier and cannot see a concurrent
    /// delete; this is where the rule actually holds.
    #[tokio::test]
    async fn the_roster_is_never_emptied() {
        let d = Directory::in_memory();
        d.add_superadmin("a@x.test", "boot").await.unwrap();
        d.add_superadmin("b@x.test", "boot").await.unwrap();
        assert!(d.remove_superadmin("a@x.test").await.unwrap());
        // Both callers saw two administrators; the second one is refused here.
        assert!(!d.remove_superadmin("b@x.test").await.unwrap());
        assert!(d.is_superadmin("b@x.test").await.unwrap(), "the row is restored, not lost");
        let left = d.superadmins().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].added_by, "boot", "restored exactly as it was");
        // Removing somebody already gone is the end state the caller asked for.
        assert!(d.remove_superadmin("a@x.test").await.unwrap());
    }
}
