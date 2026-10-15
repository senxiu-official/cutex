"""Browser-only transport/journal policy; no authority or provider operations."""
from copy import deepcopy

BROWSER_TOOLS = frozenset({
    'browser_list', 'browser_open', 'browser_claim', 'browser_read', 'browser_act',
    'browser_screenshot', 'browser_navigate', 'browser_release', 'browser_find',
})


def journal_result(name: str, result: dict) -> dict:
    """Keep images in canonical history/items, not the execution replay journal.

    Text-only and non-browser tools retain exactly their prior journal shape.
    This is logical data minimization, not secure erasure of SQLite/WAL bytes.
    """
    if name not in BROWSER_TOOLS:
        return result
    image_fields = [key for key in ('content', 'mcpContent')
                    if isinstance(result.get(key), list)
                    and any(isinstance(b, dict) and b.get('type') == 'image' for b in result[key])]
    if not image_fields:
        return result
    saved = deepcopy(result)
    for key in image_fields:
        saved[key] = [b for b in saved[key] if not (isinstance(b, dict) and b.get('type') == 'image')]
    saved['browserImagesOmitted'] = True
    return saved
