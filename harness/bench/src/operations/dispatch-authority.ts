const TOKEN = Symbol("dispatch-authorization");

export type DispatchClaims = {
  operationId: string;
  stepId: string;
  capability: string;
  version: string;
  payloadDigest: string;
  attempt: number;
};

export type DispatchToken = { readonly [TOKEN]: true };

const sameClaims = (left: DispatchClaims, right: DispatchClaims): boolean =>
  left.operationId === right.operationId &&
  left.stepId === right.stepId &&
  left.capability === right.capability &&
  left.version === right.version &&
  left.payloadDigest === right.payloadDigest &&
  left.attempt === right.attempt;

export class DispatchAuthority {
  readonly #issued = new WeakMap<object, DispatchClaims & { generation: number }>();
  readonly #generations = new Map<string, { generation: number; token: object }>();

  #key(operationId: string, stepId: string): string {
    return `${operationId}\0${stepId}`;
  }

  issue(binding: DispatchClaims): DispatchToken {
    const key = this.#key(binding.operationId, binding.stepId);
    const generation = (this.#generations.get(key)?.generation ?? 0) + 1;
    const token = Object.freeze({}) as DispatchToken;
    this.#generations.set(key, { generation, token });
    this.#issued.set(token, { ...binding, generation });
    return token;
  }

  invalidate(operationId: string, stepId: string): void {
    const key = this.#key(operationId, stepId);
    this.#generations.delete(key);
  }

  consume(token: DispatchToken, claims: DispatchClaims): boolean {
    const binding = this.#issued.get(token);
    if (!binding) return false;
    this.#issued.delete(token);
    const key = this.#key(binding.operationId, binding.stepId);
    const current = this.#generations.get(key);
    if (current?.token === token) this.#generations.delete(key);
    return current?.generation === binding.generation && current.token === token && sameClaims(binding, claims);
  }
}
