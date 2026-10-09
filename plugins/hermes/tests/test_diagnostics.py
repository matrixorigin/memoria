import json

import pytest
from memoria_hermes.diagnostics import main
from memoria_hermes.outbox import Outbox


def test_diagnostics_never_prints_payload_and_retry_requires_ack(tmp_path, capsys):
    box = Outbox(tmp_path / "plugin-data" / "memoria")
    box.enqueue("binding", {"content": "private conversation"}, "history")
    row = box.claim("binding")
    event = row["id"]
    box.finish(event, "uncertain", "network_result_unknown", claim_token=row["claim_token"])
    main(["--home", str(tmp_path)])
    output = capsys.readouterr().out
    assert "private conversation" not in output
    assert json.loads(output)["needs_attention"][0]["state"] == "uncertain"
    with pytest.raises(SystemExit):
        main(["--home", str(tmp_path), "--retry", event])
    assert box.counts("binding") == {"uncertain": 1}
    main(["--home", str(tmp_path), "--retry", event, "--acknowledge-duplicate-risk"])
    assert box.counts("binding") == {"pending": 1}
    main(["--home", str(tmp_path), "--discard", event])
    assert box.counts("binding") == {"discarded": 1}
    # Discard retains the receipt, so a duplicate callback doesn't resurrect it.
    assert not box.enqueue("binding", {"content": "private conversation"}, "history")
