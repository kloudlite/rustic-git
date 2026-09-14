import { test } from "node:test";
import net from "node:net";
import { requestsSurvive } from "./collector.ts";

test("a collector that accepts and never replies never fails or stalls a request", async () => {
  const hung = net.createServer(() => {}).listen(0, "127.0.0.1");
  await new Promise((ok) => hung.once("listening", ok));
  await requestsSurvive(`http://127.0.0.1:${(hung.address() as net.AddressInfo).port}`);
  hung.close();
  process.exit(0); // the exporter's pending socket to the hung collector would hold the loop open
});
