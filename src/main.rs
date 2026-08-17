use rustic_git::{store::Store, App, Result};
use std::sync::Arc;

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// Read `[profile]` from an AWS INI file; returns key=value pairs.
fn aws_ini(file: &str, profile: &str) -> Vec<(String, String)> {
    let Some(home) = std::env::var_os("HOME") else {
        return vec![];
    };
    let Ok(text) = std::fs::read_to_string(std::path::Path::new(&home).join(".aws").join(file))
    else {
        return vec![];
    };
    let mut out = vec![];
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let name = line.trim_matches(&['[', ']'][..]).trim();
            inside = name == profile || name == format!("profile {profile}");
        } else if inside {
            if let Some((k, v)) = line.split_once('=') {
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

/// If no AWS_ACCESS_KEY_ID in env, export credentials/region from ~/.aws for $AWS_PROFILE (default "default").
fn load_aws_profile() {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
        return;
    }
    let profile = env("AWS_PROFILE", "default");
    let map = [
        ("aws_access_key_id", "AWS_ACCESS_KEY_ID"),
        ("aws_secret_access_key", "AWS_SECRET_ACCESS_KEY"),
        ("aws_session_token", "AWS_SESSION_TOKEN"),
        ("region", "AWS_REGION"),
        ("endpoint_url", "AWS_ENDPOINT"),
    ];
    for (k, v) in aws_ini("credentials", &profile)
        .into_iter()
        .chain(aws_ini("config", &profile))
    {
        if let Some((_, env_key)) = map.iter().find(|(ini, _)| *ini == k) {
            if std::env::var_os(env_key).is_none() {
                std::env::set_var(env_key, v);
            }
        }
    }
    // ponytail: static keys + region only; SSO/assume-role profiles need the AWS SDK credential chain
}

fn object_store() -> Result<Arc<dyn slatedb::object_store::ObjectStore>> {
    load_aws_profile();
    let url = std::env::var("RUSTIC_GIT_S3_URL").map_err(|_| {
        rustic_git::err("RUSTIC_GIT_S3_URL required (e.g. s3://bucket, or mem:// for testing)")
    })?;
    let os: Arc<dyn slatedb::object_store::ObjectStore> = if url == "mem://" {
        Arc::new(slatedb::object_store::memory::InMemory::new())
    } else if let Some(bucket) = url.strip_prefix("s3://") {
        // Built by hand rather than via resolve_object_store so the request timeout can be
        // raised: repack uploads a whole repository in one PUT, and object_store's 180s default
        // aborts that on a slow or distant link.
        use slatedb::object_store::{aws::AmazonS3Builder, ClientOptions};
        let timeout = env("RUSTIC_GIT_S3_TIMEOUT_SECS", "900")
            .parse()
            .map_err(|_| rustic_git::err("RUSTIC_GIT_S3_TIMEOUT_SECS must be a number"))?;
        let mut b = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_client_options(
                ClientOptions::new().with_timeout(std::time::Duration::from_secs(timeout)),
            );
        if let Ok(ep) = std::env::var("AWS_ENDPOINT") {
            b = b.with_endpoint(ep).with_virtual_hosted_style_request(false);
        }
        Arc::new(b.build()?)
    } else {
        slatedb::Db::resolve_object_store(&url)?
    };
    Ok(os)
}

async fn open_store(background: bool) -> Result<Arc<Store>> {
    Ok(Arc::new(
        Store::open(
            object_store()?,
            env("RUSTIC_GIT_CACHE_DIR", "./cache").into(),
            background,
        )
        .await?,
    ))
}

// ponytail: no CryptoRng impl for OsRng is reachable through the rand_core
// version russh/ssh-key 0.7.0-rc.11 pin (0.10.1, which has no OsRng at all);
// shell out to ssh-keygen (present on any host running sshd) instead of
// pulling in a duplicate rand_core dependency just for key generation.
fn host_key(path: &str) -> Result<russh::keys::PrivateKey> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(p)
            .status()?;
        if !status.success() {
            return Err(rustic_git::err("ssh-keygen failed to generate host key"));
        }
    }
    Ok(russh::keys::PrivateKey::read_openssh_file(p)?)
}

/// Start the server. This node opens whatever repo the balancer sends it and holds it warm
/// afterwards. Nothing is elected here: which node serves a repo is the balancer's decision, and
/// it must route a repo to exactly one node, or the second opener fences the first.
async fn serve() -> Result<()> {
    let store = Arc::new(
        Store::open(
            object_store()?,
            env("RUSTIC_GIT_CACHE_DIR", "./cache").into(),
            true,
        )
        .await?,
    );
    let app = Arc::new(App::new(store.clone()));
    store.pool.spawn_sweeper();
    let http = tokio::net::TcpListener::bind(env("RUSTIC_GIT_HTTP_ADDR", "0.0.0.0:8080")).await?;
    let ssh = tokio::net::TcpListener::bind(env("RUSTIC_GIT_SSH_ADDR", "0.0.0.0:2222")).await?;
    let key = host_key(&env("RUSTIC_GIT_HOST_KEY", "./host_key"))?;
    eprintln!(
        "http on {} ssh on {} — up to {} warm databases",
        http.local_addr()?,
        ssh.local_addr()?,
        store.pool.max_warm,
    );
    let a2 = app.clone();
    tokio::select! {
        r = axum::serve(http, rustic_git::http::router(a2)) => { r?; }
        r = rustic_git::ssh::serve(app, ssh, key) => { r?; }
    }
    store.pool.close().await;
    Ok(())
}

async fn run(a: &[&str], store: &Arc<Store>) -> Result<()> {
    match a {
        ["serve"] => serve().await,
        ["admin", "fork", src, dst] => {
            let (so, sn) = src.split_once('/').ok_or("owner/name")?;
            let (o, n) = dst.split_once('/').ok_or("owner/name")?;
            let src = store.open_repo(so, sn).await?.ok_or("source repository not found")?;
            store.fork(&src, o, n).await
        }
        ["admin", "repack", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            let (before, after) = store.repack(o, n).await?;
            println!("repacked {path}: {before} packs -> {after}");
            Ok(())
        }
        ["admin", "delete-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            store.delete_repo(o, n).await
        }
        ["admin", "create-repo", path] => {
            let (o, n) = path.split_once('/').ok_or("owner/name")?;
            store.create_repo(o, n).await
        }
        ["admin", "add-token", owner] => {
            println!("{}", store.create_token(owner).await?);
            Ok(())
        }
        ["admin", "add-key", owner, file] => {
            store.add_ssh_key(owner, &std::fs::read_to_string(file)?).await
        }
        _ => Err(rustic_git::err(
            "usage: rustic-git serve | admin create-repo <owner>/<name> | admin fork <src>/<name> <owner>/<name> | admin delete-repo <owner>/<name> | admin repack <owner>/<name> | admin add-token <owner> | admin add-key <owner> <pubkey-file>",
        )),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if a.first() == Some(&"serve") {
        let r = serve().await;
        if let Err(e) = r {
            eprintln!("{e}");
            std::process::exit(2);
        }
        return Ok(());
    }
    let store = open_store(false).await?;
    let r = run(&a, &store).await;
    store.pool.close().await;
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(2);
    }
    Ok(())
}
