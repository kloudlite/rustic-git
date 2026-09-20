import assert from "node:assert/strict";
import { test } from "node:test";
import { DispatchAuthority } from "../src/operations/dispatch-authority.ts";

const binding = {
  operationId: "op-1",
  stepId: "step-1",
  capability: "file.edit",
  version: "1.0.0",
  payloadDigest: `sha256:${"a".repeat(64)}`,
  attempt: 1,
};

test("only the issuing authority accepts an opaque dispatch token", () => {
  const issuer = new DispatchAuthority();
  const foreign = new DispatchAuthority();
  const token = issuer.issue(binding);

  assert.equal(foreign.consume(token, binding), false);
  assert.equal(issuer.consume({} as typeof token, binding), false);
  assert.equal(issuer.consume(token, binding), true);
  assert.equal(issuer.consume(token, binding), false);
});

test("the first consumption attempt burns a dispatch token", () => {
  const authority = new DispatchAuthority();
  const token = authority.issue(binding);

  assert.equal(authority.consume(token, { ...binding, payloadDigest: `sha256:${"b".repeat(64)}` }), false);
  assert.equal(authority.consume(token, binding), false);
});

test("issuing a new attempt invalidates every outstanding token for the step", () => {
  const authority = new DispatchAuthority();
  const first = authority.issue({ ...binding, attempt: 1 });
  const second = authority.issue({ ...binding, attempt: 2 });

  assert.equal(authority.consume(first, { ...binding, attempt: 1 }), false);
  assert.equal(authority.consume(second, { ...binding, attempt: 2 }), true);
});

test("invalidating a step burns its outstanding dispatch token", () => {
  const authority = new DispatchAuthority();
  const token = authority.issue(binding);

  authority.invalidate(binding.operationId, binding.stepId);

  assert.equal(authority.consume(token, binding), false);
});
