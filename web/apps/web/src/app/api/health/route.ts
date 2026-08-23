/* Probe target. The kubelet hits this every few seconds per replica; rendering /login for
   that was the most expensive thing this server did all day. No auth, no data, no body. */
export function GET() {
  return new Response(null, { status: 204 });
}
