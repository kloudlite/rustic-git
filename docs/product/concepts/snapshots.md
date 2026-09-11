# Snapshots

A snapshot captures all of an environment's persisted data — every database,
queue, and volume in it — as a single object. An environment can be cloned from a
snapshot and comes up with exactly that state.

## The gap it closes

State is the expensive part of an environment. Starting the shop's six services is
a matter of starting containers. What is hard is the data: the orders in Postgres,
the events in the queue, the uploaded receipts on a volume — seeded, migrated,
consistent with each other, and not corrupted by whatever ran last.

That expense is why teams share one staging environment, why tests that write to
the database run one at a time, and why every test suite drags a tail of fixtures,
seed scripts, and cleanup jobs behind it.

A snapshot makes state copyable. Copyable state is cheap state, and cheap state
means an environment is no longer something to protect — it is something to clone.

## One snapshot, not one per service

The snapshot is of the whole environment at once, not a backup per database to
reassemble later.

That matters because the shop's state only makes sense across services together.
A refund is a row in Postgres *and* a message in the queue *and* a status `worker`
has not yet updated. Snapshotting Postgres on Monday and the queue on Tuesday
gives you a shop that has never existed. A snapshot is one consistent picture of
all of it.

## What it changes

The refund fix needs an order over the threshold to test against. Without
snapshots you seed one into staging, run the refund, and delete it afterwards —
and hope nobody else's test saw it in between.

With snapshots: take a snapshot of your environment once, with the large order in
it, and name it `large-order-pending`. From then on, every time you — or a test,
or an agent — need that state, clone an environment from the snapshot, do whatever
you like to it, and discard it.

**Tests may mutate freely.** The refund test writes to Postgres and drains the
queue. Fine — it is a clone. The next run clones the same snapshot and starts from
the same state, so a failure is about the code, never about what ran before.

**Tests may run concurrently.** Five agents each testing a different `payments`
change get five clones of `large-order-pending`, running at the same time, none
touching the others' data. Nothing to serialize.

**Teardown disappears.** There is nothing to clean up. The clone is discarded.
The seed script becomes "clone from snapshot".

**States worth returning to are kept, not rebuilt.** The shop as it was before the
`orders` table migration. The exact data a customer's bug reproduces on. A clean
demo dataset. Each is a snapshot, restored in one step.

## Where next

- [Environments](environments.md) — what a snapshot is a snapshot of.
- [Best practices: snapshots and testing](../best-practices.md#snapshots-and-testing)
- [Environment API](../reference/api/environments.md)
