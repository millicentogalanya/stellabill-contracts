## Global subscriber credit limits

Subscribers can hold multiple subscriptions in the vault, potentially across many plans and
merchants. To prevent overextension, the vault supports per-subscriber credit limits that
bound aggregate exposure across subscriptions for a given settlement token.

### Data model

Credit limits are configured per `(subscriber, token)` pair:

- `limit` – maximum allowed exposure in token base units (e.g. 1 USDC = 1_000_000 for 6 decimals).
- `limit = 0` – no limit is enforced for that subscriber/token.

Exposure is computed as:

- `sum(prepaid_balance)` for all subscriptions owned by the subscriber using the token, plus
- `sum(amount)` for subscriptions in the `Active` state (the next-interval liability).

This provides a conservative bound on how much value is either already locked or expected to be
charged in the near term.

### Configuration entrypoints

- `set_subscriber_credit_limit(admin, subscriber, token, limit)`  
  Sets or updates the credit limit for a `subscriber` and settlement `token`. Only the contract
  admin may call this. Passing `limit = 0` clears any effective cap (no limit).

- `get_subscriber_credit_limit(subscriber, token) -> i128`  
  Returns the configured limit, or `0` when none is set.

### Enforcement points

Credit limits are enforced before new liabilities are introduced:

- **Subscription creation**
  - `create_subscription`
  - `create_subscription_with_token`
  - `create_subscription_from_plan`
  For each of these, the vault checks that `current_exposure + amount <= limit`. If the
  limit would be exceeded, the operation returns `Error::CreditLimitExceeded` and no
  state changes occur.

- **Top-ups**
  - `deposit_funds`
  Deposits increase exposure by the deposit amount. The contract rejects deposits that would
  cause `current_exposure + deposit_amount` to exceed the configured limit.

Existing subscriptions continue to function (charges, cancellations, withdrawals) even when the
limit is reached; only new subscriptions and additional prepaid exposure are blocked.

### View helpers

- `get_subscriber_exposure(subscriber, token) -> i128`  
  Returns the current aggregate exposure used for limit checks. This is suitable for dashboards
  and pre-flight checks in frontends.

### UX and risk policy guidance

- **Choosing limits**
  - For conservative risk, set limits close to the expected total recurring liability across
    all plans (e.g. 1–3 billing intervals worth of charges).
  - For high-trust subscribers, either use a large limit or `0` (no limit).

- **Frontend behavior**
  - Before attempting subscription creation or deposit, frontends can fetch both
    `get_subscriber_credit_limit` and `get_subscriber_exposure` to give users clear guidance
    on how much additional exposure is allowed.
  - When `Error::CreditLimitExceeded` is returned, surface the limit and current exposure to
    explain why the operation failed and what adjustments are needed (e.g. lower amount, cancel
    other subscriptions, or raise the limit).

