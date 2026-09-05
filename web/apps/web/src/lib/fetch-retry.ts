// A pod rolling out answers 502/503 for the second or two its replacement takes to become
// ready. One retry turns that into a served page. Reads only: a mutation that reached the api
// and lost its answer must not be sent twice.
// ponytail: fixed 300 ms, one attempt; jittered backoff when a profile says so.
export async function fetchRetrying(url: string, init: RequestInit): Promise<Response> {
  const res = await fetch(url, init);
  if ((init.method ?? "GET") !== "GET" || (res.status !== 502 && res.status !== 503)) return res;
  await new Promise((r) => setTimeout(r, 300));
  return fetch(url, init);
}
