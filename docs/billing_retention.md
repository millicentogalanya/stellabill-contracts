# Billing statement retention and compaction

This contract supports bounded billing-statement growth using configurable retention and explicit compaction.

## Retention model

- Detailed rows are append-only per subscription, keyed by sequence.
- `set_billing_retention(admin, keep_recent)` configures the default number of detailed rows to keep.
- `get_billing_retention()` returns current policy.

Default policy keeps all rows until retention is explicitly set.

## Compaction flow

- `compact_billing_statements(admin, subscription_id, keep_recent_override)`
- Admin-only operation
- Prunes oldest detailed rows beyond keep threshold
- Preserves high-level auditability in `BillingStatementAggregate`:
  - `pruned_count`
  - `total_amount`
  - `oldest_period_start`
  - `newest_period_end`

Read aggregate with `get_stmt_compacted_aggregate(subscription_id)`.

## Guarantees and limits

- Compaction never mutates remaining detailed rows.
- Remaining rows preserve sequence IDs and ordering.
- Pagination APIs continue to return stable ordering over active rows.
- Pruned detail is intentionally irreversible; only aggregate totals remain.

## Guidance

- Choose `keep_recent` to match frontend history window (for example, 12-24 periods).
- Run compaction periodically for high-volume subscriptions.
