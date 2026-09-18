# R07 checkout recovery validation

Validated on 2026-09-18 in a disposable KVM guest inside the development pod.

- Source base: `fd64e1fd0ddb85b7d5dc135ce14007fdaa5368ec`, with the accompanying uncommitted R07 test changes.
- Test source SHA-256: `eb05dc9aae90034f3b8982de71bae994cc317f36c2ccd96c1e5a2d7de2adf8e7`.
- Compilation: development container, `cargo test -p kloudlite-workspaces --test engine_snapshot --locked --no-run`.
- Execution: compiled `engine_snapshot` binary, `r07_ --ignored --test-threads=1 --nocapture`.
- Guest: Linux 6.6.142-0-virt, Btrfs 6.8.1 tools, isolated loopback filesystems.
- Result: **3 passed, 0 failed, 0 ignored**. Guest exited with result 0 and powered down.
- Astra source review passed after fixing the UID1000 child working directory.

The tests seed the durable state after interrupted creation, force a real read-only-filesystem ownership failure, and retry through a fresh Engine. They verify UID/GID1000 ownership and an actual UID1000 write; reject directory/file/symlink staging paths without changing sentinel data; and preserve source root/child ownership through snapshot checkout. This is recovery from seeded interruption state, not a power-loss durability test.

The guest used its own kernel and temporary loopback images. No production filesystem or host kernel module was changed. Logs: [results](results.log), [guest boot and execution](guest.log).
