use crate::protocol::{receive, upload};
use crate::{App, Result};
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::io::SyncIoBridge;

pub async fn serve(
    app: Arc<App>,
    listener: tokio::net::TcpListener,
    host_key: PrivateKey,
) -> Result<()> {
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        inactivity_timeout: Some(std::time::Duration::from_secs(600)),
        ..Default::default()
    });
    SshServer { app }.run_on_socket(config, &listener).await?;
    Ok(())
}

struct SshServer {
    app: Arc<App>,
}

impl Server for SshServer {
    type Handler = Conn;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Conn {
        Conn {
            app: self.app.clone(),
            owner: None,
            channels: HashMap::new(),
            env_v2: HashMap::new(),
        }
    }
}

struct Conn {
    app: Arc<App>,
    owner: Option<String>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// GIT_PROTOCOL=version=2 is per-channel: one channel setting it must not make a later
    /// channel on the same connection look v2-capable.
    env_v2: HashMap<ChannelId, bool>,
}

/// A client must not be able to park unbounded channels on one connection.
const MAX_CHANNELS: usize = 16;

impl Handler for Conn {
    type Error = crate::Error;

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        match self.app.store.owner_for_fingerprint(&fp).await? {
            Some(o) => {
                self.owner = Some(o);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        if self.channels.len() >= MAX_CHANNELS {
            reply
                .reject(russh::ChannelOpenFailure::ResourceShortage)
                .await;
            return Ok(());
        }
        reply.accept().await;
        self.channels.insert(channel.id(), channel);
        Ok(())
    }

    async fn env_request(
        &mut self,
        id: ChannelId,
        name: &str,
        value: &str,
        _session: &mut Session,
    ) -> Result<()> {
        if name == "GIT_PROTOCOL" && value.contains("version=2") {
            self.env_v2.insert(id, true);
        }
        Ok(())
    }

    // a client that opens channels and abandons them must not accumulate state on the server
    async fn channel_close(&mut self, id: ChannelId, _session: &mut Session) -> Result<()> {
        self.channels.remove(&id);
        self.env_v2.remove(&id);
        Ok(())
    }

    // only `git-upload-pack`/`git-receive-pack` exec is supported: no shells, no subsystems.
    async fn shell_request(&mut self, id: ChannelId, session: &mut Session) -> Result<()> {
        session.channel_failure(id)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(id)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let Some(channel) = self.channels.remove(&id) else {
            session.channel_failure(id)?;
            return Ok(());
        };
        session.channel_success(id)?;
        let handle = session.handle();
        let cmd = String::from_utf8_lossy(data).to_string();
        let app = self.app.clone();
        let auth_owner = self.owner.clone();
        let v2 = self.env_v2.remove(&id).unwrap_or(false);
        tokio::spawn(async move {
            if let Err(e) = run(app, auth_owner, &cmd, v2, channel, &handle, id).await {
                let _ = handle
                    .extended_data(id, 1, format!("rustic-git: {e}\n").into_bytes())
                    .await;
                let _ = handle.exit_status_request(id, 1).await;
                let _ = handle.eof(id).await;
                let _ = handle.close(id).await;
            }
        });
        Ok(())
    }
}

/// "git-upload-pack 'owner/name.git'" → ("git-upload-pack", "owner/name.git")
fn parse_cmd(cmd: &str) -> Option<(&str, &str)> {
    let (svc, rest) = cmd.trim().split_once(' ')?;
    if svc != "git-upload-pack" && svc != "git-receive-pack" {
        return None;
    }
    Some((svc, rest.trim().trim_matches(['\'', '"'])))
}

async fn run(
    app: Arc<App>,
    auth_owner: Option<String>,
    cmd: &str,
    v2: bool,
    channel: Channel<Msg>,
    handle: &russh::server::Handle,
    id: ChannelId,
) -> Result<()> {
    let (service, path) = parse_cmd(cmd).ok_or_else(|| crate::err("unsupported command"))?;
    let (owner, name) =
        crate::protocol::parse_repo_path(path).ok_or_else(|| crate::err("invalid repo path"))?;
    if !crate::auth::authorize(auth_owner.as_deref(), &owner) {
        return Err(crate::err("access denied"));
    }
    if service == "git-upload-pack" && !v2 {
        return Err(crate::err("protocol v2 required"));
    }
    let repo_path = format!("{owner}/{name}");
    match app.route(&repo_path).await {
        crate::peers::Route::Local => {} // fall through to the local path below
        crate::peers::Route::Unavailable => {
            return Err(crate::err(
                "no node may safely serve this repository right now; retry",
            ))
        }
        crate::peers::Route::Peer(peer) => {
            let authed = auth_owner.clone().unwrap_or_default();
            // The stream lives until after the exit status is sent: dropping it closes the channel.
            let mut stream = channel.into_stream();
            let piped = crate::proxy::stream_to_peer(
                &app.forwarder.secret,
                &crate::proxy::stream_addr(&peer.addr),
                service,
                &repo_path,
                &authed,
                0,
                &mut stream,
                false,
            )
            .await;
            let code = match &piped {
                Ok(()) => 0,
                Err(e) => {
                    let _ = handle
                        .extended_data(id, 1, format!("rustic-git: {e}\n").into_bytes())
                        .await;
                    1
                }
            };
            let _ = handle.exit_status_request(id, code).await;
            // No explicit handle.eof(): copy_bidirectional's shutdown already sent the channel EOF
            // through ChannelStream::poll_shutdown, and a second EOF is a protocol error.
            drop(stream);
            // Ok, not `piped`: this arm has already reported the outcome to the client. Returning
            // Err would make exec_request's caller report it AGAIN — a second stderr line, a second
            // exit status, and a second EOF.
            return Ok(());
        }
    }
    let repo = app
        .store
        .open_repo(&owner, &name)
        .await?
        .ok_or_else(|| crate::err("repository not found"))?;
    let store = app.store.clone();
    let upload = service == "git-upload-pack";
    let (rd, wr) = tokio::io::split(channel.into_stream());
    // The bridges are handed back out of the blocking task: dropping the channel stream
    // closes the SSH channel, and the exit status must go out before that.
    // On SSH a vanished client surfaces as a write error on the channel, which already aborts the
    // pack build; this flag covers the rest (and matches the HTTP path's contract).
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    let (res, input, output) = tokio::task::spawn_blocking(move || {
        let interrupt = interrupt;
        let mut input = std::io::BufReader::new(SyncIoBridge::new(rd));
        let mut output = SyncIoBridge::new(wr);
        let res = (|| -> Result<()> {
            use std::io::Write;
            if upload {
                upload::advertise(&mut output)?;
                upload::serve(&store, &repo, &mut input, &mut output, &interrupt)?;
            } else {
                receive::advertise(&store, &repo, &mut output)?;
                receive::serve(&store, &repo, &mut input, &mut output, &interrupt)?;
            }
            output.flush()?;
            Ok(())
        })();
        (res, input, output)
    })
    .await?;
    let code = match res {
        Ok(()) => 0,
        Err(e) => {
            let _ = handle
                .extended_data(id, 1, format!("rustic-git: {e}\n").into_bytes())
                .await;
            1
        }
    };
    let _ = handle.exit_status_request(id, code).await;
    let _ = handle.eof(id).await;
    drop(input);
    drop(output);
    Ok(())
}

/// Run one git service over an established byte stream, to completion. Used by the peer stream
/// path; the local SSH path keeps its own ordering in `run` because it must send an exit status
/// before its stream closes.
pub async fn serve_git<S>(
    store: Arc<crate::store::Store>,
    repo: crate::store::Repo,
    service: &str,
    stream: S,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let upload = service == "git-upload-pack";
    let (rd, wr) = tokio::io::split(stream);
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let interrupt = interrupt;
        let mut input = std::io::BufReader::new(SyncIoBridge::new(rd));
        let mut output = SyncIoBridge::new(wr);
        use std::io::Write;
        if upload {
            upload::advertise(&mut output)?;
            upload::serve(&store, &repo, &mut input, &mut output, &interrupt)?;
        } else {
            receive::advertise(&store, &repo, &mut output)?;
            receive::serve(&store, &repo, &mut input, &mut output, &interrupt)?;
        }
        output.flush()?;
        Ok(())
    })
    .await?
}
