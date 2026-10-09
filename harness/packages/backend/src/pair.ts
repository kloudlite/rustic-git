//! Test helper: a RemoteBackend talking to `serve(backend)` over an in-memory wire, so two "TUIs"
//! can share one LocalBackend without a socket. Never imported by product code.
import type { Backend } from "./index";
import { Peer } from "./wire";
import { serve } from "./serve";
import { RemoteBackend } from "./remote";

export async function pair(backend: Backend): Promise<RemoteBackend> {
  const enc = new TextEncoder();
  const nl = (l: string) => enc.encode(l.endsWith("\n") ? l : `${l}\n`);
  let client!: Peer;
  const server = new Peer((l) => client.feed(nl(l)));
  client = new Peer((l) => server.feed(nl(l)));
  serve(backend, server);
  const remote = new RemoteBackend(client);
  // connect() does the same start-up after constructing the backend
  await remote.init();
  return remote;
}
