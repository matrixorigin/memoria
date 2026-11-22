# Complete observation

`POST /v1/observe/complete` accepts the same request and returns the same
receipt as `POST /v1/observe`. Use it when extraction needs the complete
conversation, such as a correction to an earlier instruction. It requires
memory write access. Supplying exclusions also requires memory read access.

Provide 1 to 20 nonempty text messages with the role `user` or `assistant`.
The full extraction input must fit 6,000 UTF-8 bytes, including each
`[role]: ` prefix and trailing newline. Exceeding either limit returns HTTP
422 before extraction or persistence. Eligible text reaches extraction in
full. Raw fallback still stores each message individually, so clients that
need one fallback record should supply the exchange as a single message.

Older servers do not implement this route. A client that requires complete
evidence must not retry that request against the ordinary observe endpoint.

## Isolated user guidance

`POST /v1/observe/standalone` uses the same authorization and byte budget.
Send one user message when earlier private context has been omitted. The
extractor is asked to accept only a self-contained durable statement or return
no candidates. The service requires an exact copy of the full user text before
persisting an inferred memory. This prevents a paraphrase or omission of a
negation from being stored as the user's own statement. Semantic eligibility
is still a model judgment. Unavailable extraction returns HTTP 503. This
route never falls back to persisting the raw message.
