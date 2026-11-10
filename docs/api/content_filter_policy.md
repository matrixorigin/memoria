# Content filtering for trusted owner-scoped adapters

Memoria's built-in sensitivity filter rejects credential-shaped memory text and
redacts recognized personal-information patterns before embedding and storage.
This remains the default for every write path.

An operator running a trusted adapter that must preserve its input can set:

```text
MEMORIA_OWNER_CONTENT_FILTER_ENABLED=false
```

The default is `true`. Accepted values are `true`/`false` (case insensitive) and
`1`/`0`. Missing or invalid values keep filtering enabled; invalid values produce
a warning without echoing the supplied value. Restart the API processes after
changing this setting and configure all replicas consistently.

This is **not a global disable switch**. It applies only to inline REST work under a
successfully authenticated `Authorization: Memoria-Owner <master-key>` request,
with exactly one valid `X-User-Id`. The override is bound to that resolved owner.
Ordinary Bearer master/API keys, group requests, unauthenticated callers, embedded
MCP/CLI work, and spawned background jobs retain the built-in filter. Query
parameters, JSON fields, and extra headers cannot disable filtering. All trusted
adapters using this authentication scheme on the deployment share this setting;
it does not distinguish multiple adapters by name. Keep it enabled or use a
separate API deployment when trusted adapters require different policies.

The remote `POST /mcp` endpoint also keeps the built-in filter, even for
`Memoria-Owner` requests when this setting is `false`. Use the REST write
endpoints for content preservation; this setting does not opt MCP tools out.

With filtering disabled for the authenticated owner, synchronous store (including
idempotent writes), correction, batch, observe, and pipeline paths neither reject
content based on the built-in patterns nor replace recognized text. The exact
same content reaches embedding and persistence. Authentication, authorization,
owner isolation, payload-size validation, and other governance checks continue
to apply. Rejections with filtering enabled log only fixed rule names, never
memory text or matched substrings.

The policy is request-local and is restored even if the request is cancelled.
Spawned jobs do not inherit the override. Their calls to the policy-aware memory
write paths use the built-in filter. This does **not** re-filter content that was
already accepted or stored without filtering.

With filtering disabled, original text, including any credential-shaped strings,
is sent to the configured embedding provider, persisted, and included in the
memory edit log. It may also be processed as-is by background consumers such as
entity extraction, which can send text to the configured LLM and store graph
entities. Later retrieval can return the original text to callers authorized
for that owner. Embedding and LLM providers may be external services; the
background-job default is not a guarantee that accepted text stays away from
those providers. The setting controls write-time pattern filtering, not
downstream data-flow restrictions.

Changing the setting does not recover previously redacted or rejected memories.
Idempotent retries return existing receipts; they do not rewrite stored content.
Before changing a registered evaluation deployment, disclose the behavior change
and agree on a compatible replay/checkpoint policy. Do not clear existing data
or restart a formal run as part of changing this option.
