//! `kl ws proxy` is ssh's ProxyCommand: whatever ssh writes must come back out of stdout. The
//! stub tunnel echoes, so a round trip through the real binary is the whole test.

mod stub;

use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[test]
fn pumps_stdin_to_the_tunnel_and_back_to_stdout() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _g = rt.enter();
    let api = rt.block_on(stub::spawn(stub::Stub));
    let cfg = tempfile::tempdir().unwrap();
    stub::write_config(cfg.path(), &api);

    let mut child = Command::new(env!("CARGO_BIN_EXE_kl"))
        .args(["ws", "proxy", "ws-1"])
        .env("KL_CONFIG_DIR", cfg.path())
        .env("KL_GATEWAY_OVERRIDE", api.replace("http://", "ws://"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"SSH-2.0-kl\r\n").unwrap();
    stdin.flush().unwrap();

    let mut out = child.stdout.take().unwrap();
    let mut buf = [0u8; 12];
    out.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"SSH-2.0-kl\r\n");

    drop(stdin); // EOF ends the session
    let status = child.wait().unwrap();
    assert!(status.success(), "proxy should exit 0 on stdin EOF");
}
