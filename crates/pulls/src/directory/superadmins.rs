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

    pub async fn remove_superadmin(&self, user: &str) -> Result<()> {
        let user = user.trim().to_lowercase();
        match &self.backend {
            Backend::Mongo(m) => {
                m.superadmins
                    .delete_one(doc! { "_id": user })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
            }
            Backend::Memory(s) => {
                s.lock().unwrap().superadmins.remove(&user);
            }
        }
        Ok(())
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
