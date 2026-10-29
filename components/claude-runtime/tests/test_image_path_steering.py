"""r264 daemon image-path payload uses the existing r30 text steer protocol.

Fake SDK only; no model, credential, daemon or network connection.
"""
import asyncio
import json

from cute_claude.busy_delivery import STEER_FRAME
from test_persistent import ph  # noqa: F401
from test_questions_plan import begin
from test_perms_steer import post_hook


def path_part(path):
    return {"type": "text", "text":
            "[Image supplied as a file path on this agent's host; use your image-reading tool to inspect it. "
            f"Path is a JSON string: {json.dumps(str(path), ensure_ascii=False)}]"}


async def test_multiple_image_paths_use_same_turn_text_hook_once(ph):
    tid, turn, bridge = await begin(ph)
    original = {"type": "text", "text": "看看这两张\n原文保留"}
    parts = [original, path_part(ph.settings.workspace / "space 图片 [x] $`.png"),
             path_part(ph.settings.workspace / "second.png")]
    params = {"threadId": tid, "expectedTurnId": turn, "clientUserMessageId": "images-1", "input": parts}
    assert await ph.call("turn/steer", params) == {"turnId": turn}
    assert await ph.call("turn/steer", params) == {"turnId": turn}
    context = (await post_hook(ph, bridge))["hookSpecificOutput"]["additionalContext"]
    assert context == STEER_FRAME + "\n".join(part["text"] for part in parts)
    assert STEER_FRAME not in json.dumps(await post_hook(ph, bridge, call="tool-2"))
    await asyncio.sleep(0)
    items = [item for item in ph.store.items(turn) if item.get("clientId") == "images-1"]
    assert len(items) == 1
    assert items[0]["steered"] is True
    assert "attachments" not in json.loads(ph.store.db.execute(
        "SELECT payload FROM inputs WHERE message_id='steer-images-1'").fetchone()[0])


async def test_image_path_steer_stale_turn_is_a_definite_refusal(ph):
    tid, _, _ = await begin(ph)
    params = {"threadId": tid, "expectedTurnId": "stale", "clientUserMessageId": "images-stale",
              "input": [path_part(ph.settings.workspace / "image.png")]}
    result = await ph.call("turn/steer", params, error=True)
    assert result["code"] == -32600
    assert ph.store.db.execute("SELECT count(*) FROM inputs WHERE message_id='steer-images-stale'").fetchone()[0] == 0
