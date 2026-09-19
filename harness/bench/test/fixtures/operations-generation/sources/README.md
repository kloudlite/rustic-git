Fixtures for the O04 generation adapter (`operations-generation.test.ts`).

`config.ts` is ordinary eligible source. `credentials.env` and `id_rsa` are deliberately
secret-bearing: they are placeholders, and the deterministic scanner in `generation.ts` must
refuse them even when a policy wrongly says they are eligible. Nothing here is a real credential.
