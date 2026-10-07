//! The laptop's clipboard image, served to the bench over ssh's `-R` socket forward.
//!
//! The agent CLI on the bench pastes an image by running `xclip -t image/png -o`, and the bench
//! has no clipboard of its own: the image is on this laptop. So `bench()` listens on a local unix
//! socket, ssh forwards a remote socket to it (`-R`), and the bench's `xclip` shim
//! (bench/term/xclip) connects there on Ctrl+V. Each connection gets the clipboard's PNG bytes at
//! that moment, or nothing when it holds no image, then EOF. The bench sends nothing; there is no
//! protocol to parse.

use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;

/// Serves `sock` until the task is dropped. A failed clipboard read is an empty reply, never an
/// error: the paste just finds no image, as on a text clipboard.
pub async fn serve(listener: UnixListener) {
    loop {
        let Ok((mut conn, _)) = listener.accept().await else { continue };
        tokio::spawn(async move {
            let png = read_png().await;
            let _ = conn.write_all(&png).await;
            let _ = conn.shutdown().await;
        });
    }
}

/// Binds `path`, owner-only: anyone who can connect reads this laptop's clipboard.
pub fn listen(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// The clipboard as PNG bytes; empty when it holds no image or no clipboard tool is there.
async fn read_png() -> Vec<u8> {
    if cfg!(target_os = "macos") {
        pasteboard().await
    } else {
        let x = run("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]).await;
        if !x.is_empty() {
            return x;
        }
        run("wl-paste", &["--no-newline", "--type", "image/png"]).await
    }
}

/// macOS: `pbpaste` cannot emit PNG, so AppleScript coerces the clipboard to «class PNGf» into a
/// temp file, read back and deleted. A text clipboard fails the coercion: empty file, no image.
async fn pasteboard() -> Vec<u8> {
    let path = std::env::temp_dir().join(format!("kl-clip-{}.png", std::process::id()));
    let script = format!(
        "set f to open for access POSIX file \"{}\" with write permission\nset eof f to 0\n\
         write (the clipboard as «class PNGf») to f\nclose access f",
        path.display()
    );
    let _ = run("osascript", &["-e", &script]).await;
    let png = std::fs::read(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    png
}

/// stdout of a successful run, else empty (missing tool, non-zero exit: xclip's "no image").
async fn run(prog: &str, args: &[&str]) -> Vec<u8> {
    match tokio::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => Vec::new(),
    }
}

/// A remote socket name no other session on the bench shares: sshd refuses to bind over a stale
/// one (no `StreamLocalBindUnlink`), so every connection takes a fresh path.
pub fn remote_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/kl-clip-{}-{nanos:x}.sock", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn a_connection_gets_the_clipboard_then_eof_and_never_hangs() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("c.sock");
        let task = tokio::spawn(serve(listen(&path).unwrap()));
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        let fut = async {
            let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
            let mut got = Vec::new();
            c.read_to_end(&mut got).await.unwrap();
            // Whatever the machine's clipboard holds: an image is a PNG, anything else is nothing.
            assert!(got.is_empty() || got.starts_with(b"\x89PNG"), "{} bytes, not a PNG", got.len());
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), fut).await.expect("timed out");
        task.abort();
    }
}
