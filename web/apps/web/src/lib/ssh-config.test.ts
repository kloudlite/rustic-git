import { expect, test } from "bun:test";
import { sshConfigBlock, sshOneLiner } from "./ssh-config";

/** Byte-exact: this block is pasted into ~/.ssh/config, and it has to match what
 *  `kl ws ssh-config` writes (bins/kl/src/sshconfig.rs) or the two disagree about
 *  the same host. */
test("renders the Host block the CLI would have written", () => {
  expect(sshConfigBlock("gh", "ws-abc123")).toBe(
    "Host gh\n" +
      "  HostName ws-abc123\n" +
      "  User root\n" +
      "  ProxyCommand kl ws proxy ws-abc123\n" +
      "  UserKnownHostsFile ~/.config/kl/known_hosts\n" +
      "  HostKeyAlias ws-abc123\n",
  );
});

test("the one-liner names the workspace, not its id", () => {
  expect(sshOneLiner("gh")).toBe("kl ws ssh gh");
});
