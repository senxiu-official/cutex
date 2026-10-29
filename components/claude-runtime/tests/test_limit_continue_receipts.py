"""Offline usage-limit/Continue receipt reproduction; no model or live home."""
import json
from dataclasses import replace

import pytest

from test_subscription import sh, emit_reply
from test_r8_services import envelope, install, invoke
from cute_claude.external_input import semantic_digest
from cute_claude.subscription import SdkResponseError


@pytest.mark.parametrize("answered", [False, True])
async def test_limit_then_continue_settles_passive_notes(sh, answered):
    tid = await sh.thread()
    service = install(sh, subscription=True)
    before = envelope(tid, "before-limit", passive=True)
    during = envelope(tid, "during-limit", passive=True)
    await invoke(service, sh, "submit", before, after=False)

    async def limit(**kw):
        await invoke(service, sh, "submit", during, after=False)
        if answered:
            await emit_reply(kw["on_message"], kw["session_id"], "partial", subtype="error_max_turns", is_error=True)
        raise SdkResponseError({"error": "rate_limit", "content": []}, None)

    sh.provider.behavior = limit
    failed = await sh.turn(tid, "work")
    await sh.drain()
    assert sh.store.turn(failed)["status"] == "failed"
    sh.provider.behavior = None
    continued = await sh.turn(tid, "继续 / Continue")
    await sh.drain()
    assert sh.store.turn(continued)["status"] == "completed", sh.store.turn(continued)
    for note in (before, during):
        status = service._status(tid, note["message"]["id"], note["semanticSha256"])
        assert status["deliveryState"] == "context_persisted", status


@pytest.mark.parametrize("repeat_limit", [False, True])
async def test_limit_then_continue_settles_notes_offered_at_busy_boundary(sh, repeat_limit):
    tid = await sh.thread()
    service = install(sh, subscription=True)
    before = envelope(tid, "busy-before-limit")
    before["message"].update(delivery="soon", type="agent_message", source={"kind": "agent", "id": "peer"}, text="project services delivered")
    before["semanticSha256"] = semantic_digest(before)
    during = envelope(tid, "pending-during-limit", passive=True)

    async def limit(**kw):
        await invoke(service, sh, "submit", before, after=False)
        assert "project services delivered" in sh.server.busy_delivery.inject(tid, kw["turn_id"])
        sh.server.busy_delivery.offered(tid, kw["turn_id"])
        await invoke(service, sh, "submit", during, after=False)
        raise SdkResponseError({"error": "rate_limit", "content": []}, None)

    sh.provider.behavior = limit
    failed = await sh.turn(tid, "work")
    await sh.drain()
    assert sh.store.turn(failed)["status"] == "failed"
    attempts = 0

    async def continue_work(**kw):
        nonlocal attempts
        attempts += 1
        if repeat_limit and attempts == 1:
            raise SdkResponseError({"error": "rate_limit", "content": []}, None)
        # Carrying input into a prompt is still not an SDK acknowledgement.
        assert service._status(tid, before["message"]["id"], before["semanticSha256"])["receipt"] is None
        await emit_reply(kw["on_message"], kw["session_id"], "read notes")

    sh.provider.behavior = continue_work
    continued = await sh.turn(tid, "继续 / Continue")
    await sh.drain()
    if repeat_limit:
        assert sh.store.turn(continued)["status"] == "failed"
        assert service._status(tid, before["message"]["id"], before["semanticSha256"])["receipt"] is None
        continued = await sh.turn(tid, "Continue after the next reset")
        await sh.drain()
    assert sh.store.turn(continued)["status"] == "completed", sh.store.turn(continued)
    for note in (before, during):
        status = service._status(tid, note["message"]["id"], note["semanticSha256"])
        assert status["deliveryState"] == "context_persisted", status

    sent = json.dumps(sh.provider.requests[-1]["message"], ensure_ascii=False)
    assert sent.count("project services delivered") == 1
    assert len(sh.provider.requests) == (3 if repeat_limit else 2), "continue never re-executes the failed turn"
    for note in (before, during):
        status = service._status(tid, note["message"]["id"], note["semanticSha256"])
        assert status["receipt"]["turnId"] == continued
    assert len([i for i in sh.store.items(continued) if i["id"] == before["message"]["id"]]) == 1


    receipt = service._status(tid, before["message"]["id"], before["semanticSha256"])["receipt"]
    sh.provider.behavior = None
    await sh.turn(tid, "next instruction")
    await sh.drain()
    assert "project services delivered" not in json.dumps(sh.provider.requests[-1]["message"], ensure_ascii=False)
    assert service._status(tid, before["message"]["id"], before["semanticSha256"])["receipt"] == receipt


@pytest.mark.parametrize("exclude", ["non-quota", "service", "delegation", "unoffered", "reverted"])
async def test_continue_never_replays_other_uncertain_inputs(sh, exclude):
    tid = await sh.thread()
    service = install(sh, subscription=True)
    value = envelope(tid, "guarded")
    value["message"].update(delivery="soon", type="agent_message", source={"kind": "agent", "id": "peer"})
    if exclude == "service":
        value["message"]["source"]["kind"] = "service"
    if exclude == "delegation":
        value["message"]["type"] = "ANSWER"
    value["semanticSha256"] = semantic_digest(value)

    async def limit(**kw):
        await invoke(service, sh, "submit", value, after=False)
        assert sh.server.busy_delivery.inject(tid, kw["turn_id"])
        if exclude != "unoffered":
            sh.server.busy_delivery.offered(tid, kw["turn_id"])
        raise SdkResponseError({"error": "server_error" if exclude == "non-quota" else "rate_limit", "content": []}, None)

    sh.provider.behavior = limit
    failed = await sh.turn(tid, "work")
    await sh.drain()
    if exclude == "reverted":
        sh.store.db.execute("UPDATE turns SET reverted=1 WHERE id=?", (failed,))
        sh.store.db.execute("UPDATE threads SET halted=0 WHERE id=?", (tid,))
    sh.provider.behavior = None
    continued = await sh.turn(tid, "Continue")
    await sh.drain()
    assert sh.store.turn(continued)["status"] == "completed"
    row = sh.store.db.execute("SELECT context_turn_id,persisted FROM busy_inputs WHERE message_id='guarded'").fetchone()
    assert tuple(row) == (failed, 0)
    assert service._status(tid, "guarded", value["semanticSha256"])["receipt"] is None


async def test_quota_notes_are_carried_in_bounded_batches_without_loss(sh):
    tid = await sh.thread()
    service = install(sh, subscription=True)
    sh.server.settings = replace(sh.server.settings, max_pending=2)
    values = []

    async def limit(**kw):
        for number in range(3):
            value = envelope(tid, f"batch-{number}")
            value["message"].update(delivery="soon", type="agent_message", source={"kind": "agent", "id": "peer"})
            value["semanticSha256"] = semantic_digest(value)
            values.append(value)
            await invoke(service, sh, "submit", value, after=False)
            assert sh.server.busy_delivery.inject(tid, kw["turn_id"])
            sh.server.busy_delivery.offered(tid, kw["turn_id"])
        raise SdkResponseError({"error": "rate_limit", "content": []}, None)

    sh.provider.behavior = limit
    await sh.turn(tid, "work")
    await sh.drain()
    sh.provider.behavior = None
    first = await sh.turn(tid, "Continue")
    await sh.drain()
    def status(value):
        return service._status(tid, value["message"]["id"], value["semanticSha256"])
    assert [status(value)["deliveryState"] for value in values] == ["context_persisted", "context_persisted", "pending"]
    second = await sh.turn(tid, "Continue the remaining notes")
    await sh.drain()
    assert [status(value)["receipt"]["turnId"] for value in values] == [first, first, second]
