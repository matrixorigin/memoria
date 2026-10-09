# Cloud API acceptance — 2026-10-08

Result: **13 checks passed** against `https://api.thememoria.ai`, using the locally
provided API Key. The key was read into memory, not printed or persisted in the
disposable Hermes profiles or report. Only newly created test subjects were used.

The script loads the real Hermes MemoryManager/provider/secret-scope code at
`0240fa4a84123406a0e5e6e7262e5b772b43f0bd` and calls this plugin against the live
Cloud REST API. Session switching invokes the host lifecycle hook. This is not
a full natural-language agent conversation, desktop interaction or gateway test.

| Check | Result |
| --- | --- |
| Authenticated profile read | Passed |
| Store binds the expected subject | Passed |
| Explicit recall after session switch | Passed |
| Automatic recall with the default 2-second IO timeout | Passed |
| Profile includes the saved preference | Passed |
| A second profile cannot recall the first profile's memory | Passed |
| A second profile cannot delete the first profile's memory | Passed |
| `useful` feedback | Passed |
| Correction returns updated content | Passed |
| Deletion removes the corrected record from active recall | Passed |
| Background observe gets a confirmed `done` receipt | Passed |
| The automatically captured fact is recallable | Passed |
| Repeated capture callback does not add another event | Passed |

The measured recall call took 0.797 seconds; background sync submission took
0.003 seconds. These are observations from one acceptance run, not performance
guarantees. Capture returned no fallback warning, and its outbox ended at
`{"done": 1}`.

Cleanup deleted the corrected test memory during the test and removed one further
active record at teardown. A final list filtered by the disposable subject
returned **zero active records**. This confirms active-memory cleanup, not physical
erasure of all server audit/version history.

Reproduce deliberately with a normal user API Key file:

```sh
HERMES_SOURCE=/path/to/hermes-agent /path/to/test-venv/bin/python plugins/hermes/tests/cloud_smoke.py --key-file /path/to/local-key-file --report /tmp/memoria-cloud-acceptance.json
```

The script supports a dotenv `MEMORIA_API_KEY` variable or a single bare `sk-...`
line. It makes live writes and scoped cleanup requests. It is opt-in and is not
run by the offline pytest suite or CI.

## Review-fix retest — 2026-10-08

After the queue retry/claim-token/cache-generation fixes, the same 13 live checks
passed again. Recall took 0.717 seconds and sync submission 0.002 seconds in this
run. Capture ended at `{"done": 1}` with no fallback warning; final scoped cleanup
again found zero active test records. These measurements remain single-run observations.
