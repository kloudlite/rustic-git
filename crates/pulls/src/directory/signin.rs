//! Sign-in links and CLI device logins: short-lived rows a person redeems once, swept when they
//! expire. Both backends, Mongo and the in-memory one the tests run on.

use super::*;

impl Directory {
    /// Delete the rows every read already ignores: spent-or-stale sign-in links, CLI login
    /// codes nobody collected, invitations past their date. `cli_logins` is fed by an anonymous
    /// endpoint and would otherwise grow at whatever rate the internet pokes it.
    pub async fn sweep_expired(&self) -> Result<u64> {
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => {
                let gone = doc! { "expiresAt": { "$lt": now } };
                let mut n = 0;
                n += m.signins.delete_many(gone.clone()).await.map_err(|e| err(format!("mongo: {e}")))?.deleted_count;
                n += m.cli_logins.delete_many(gone.clone()).await.map_err(|e| err(format!("mongo: {e}")))?.deleted_count;
                n += m.invites.delete_many(gone).await.map_err(|e| err(format!("mongo: {e}")))?.deleted_count;
                Ok(n)
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                let before = s.signins.len() + s.cli_logins.len() + s.invites.len();
                s.signins.retain(|_, l| l.expires_at >= now);
                s.cli_logins.retain(|_, l| l.expires_at >= now);
                s.invites.retain(|_, i| i.expires_at >= now);
                Ok((before - (s.signins.len() + s.cli_logins.len() + s.invites.len())) as u64)
            }
        }
    }

    // ── people ──────────────────────────────────────────────────────────────

    pub async fn create_signin(&self, link: &SignInLink) -> Result<()> {
        match &self.backend {
            Backend::Mongo(m) => m
                .signins
                .insert_one(link)
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                s.lock().unwrap().signins.insert(link.id.clone(), link.clone());
                Ok(())
            }
        }
    }

    /// The email behind a link, spending it. `None` for spent, expired or made up alike.
    /// Expiry is checked in the delete filter itself, so an expired row can never be redeemed
    /// by racing the read; `sweep_expired` removes the leftovers.
    pub async fn redeem_signin(&self, id: &str) -> Result<Option<String>> {
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => m
                .signins
                .find_one_and_delete(doc! { "_id": id, "expiresAt": { "$gt": now } })
                .await
                .map(|r| r.map(|l| l.email))
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                // Expiry is part of the delete, exactly as the Mongo filter has it: an expired
                // row is left in place for the sweep rather than spent.
                match s.signins.get(id) {
                    Some(l) if l.expires_at > now => Ok(s.signins.remove(id).map(|l| l.email)),
                    _ => Ok(None),
                }
            }
        }
    }

    // ── cli logins ──────────────────────────────────────────────────────────
    //
    // Every read filters on `expiresAt`, so a stale row is inert; `sweep_expired` removes it.

    pub async fn create_cli_login(&self, l: &CliLogin) -> Result<()> {
        match &self.backend {
            Backend::Mongo(m) => m
                .cli_logins
                .insert_one(l)
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                s.lock().unwrap().cli_logins.insert(l.code.clone(), l.clone());
                Ok(())
            }
        }
    }

    /// A code still waiting for approval. `None` for approved, expired and unknown alike —
    /// callers answer all three the same way, so a guesser learns nothing.
    pub async fn cli_login_pending(&self, code: &str) -> Result<Option<CliLogin>> {
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => m
                .cli_logins
                .find_one(doc! { "_id": code, "expiresAt": { "$gt": now }, "token": null })
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s
                .lock()
                .unwrap()
                .cli_logins
                .get(code)
                .filter(|l| l.expires_at > now && l.token.is_none())
                .cloned()),
        }
    }

    /// Attach the minted token to a waiting code. `false` means it was not waiting — unknown,
    /// expired, or approved already by someone else's click. The whole check is the update's
    /// own filter, so two approvals of one code cannot both win.
    pub async fn approve_cli_login(&self, code: &str, token: &str, exp: u64) -> Result<bool> {
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => m
                .cli_logins
                .find_one_and_update(
                    doc! { "_id": code, "expiresAt": { "$gt": now }, "token": null },
                    doc! { "$set": { "token": token, "tokenExp": exp as i64 } },
                )
                .await
                .map(|r| r.is_some())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                // The lock stands in for the update's own filter: the check and the write are
                // one step, so a second approval of one code cannot also win.
                match s.cli_logins.get_mut(code) {
                    Some(l) if l.expires_at > now && l.token.is_none() => {
                        l.token = Some(token.to_string());
                        l.token_exp = exp;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
        }
    }

    /// What the CLI polls. `Ok(None)` is a poll id that names nothing live; `Some(row)` with no
    /// token is "still waiting"; `Some(row)` with a token is the token, exactly once — the
    /// delete IS the read, so a second poller finds nothing.
    pub async fn take_cli_login(&self, poll: &str) -> Result<Option<CliLogin>> {
        let now = DateTime::now();
        match &self.backend {
            Backend::Mongo(m) => {
                let live = doc! { "poll": poll, "expiresAt": { "$gt": now } };
                let mut approved = live.clone();
                approved.insert("token", doc! { "$ne": null });
                if let Some(row) = m
                    .cli_logins
                    .find_one_and_delete(approved)
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?
                {
                    return Ok(Some(row));
                }
                m.cli_logins.find_one(live).await.map_err(|e| err(format!("mongo: {e}")))
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                let Some(code) = s
                    .cli_logins
                    .values()
                    .find(|l| l.poll == poll && l.expires_at > now)
                    .map(|l| l.code.clone())
                else {
                    return Ok(None);
                };
                // Approved rows are taken exactly once — the delete IS the read.
                if s.cli_logins[&code].token.is_some() {
                    return Ok(s.cli_logins.remove(&code));
                }
                Ok(s.cli_logins.get(&code).cloned())
            }
        }
    }
}
