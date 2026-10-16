"""Display-only records in the existing protocol; never native model/tool history."""
import hashlib
from datetime import datetime

from .asks import title as ask_title


def framed(domain: bytes, fields: list[str]) -> str:
    digest = hashlib.sha256(domain)
    for field in fields:
        value = field.encode('utf-8')
        digest.update(len(value).to_bytes(8, 'big'))
        digest.update(value)
    return digest.hexdigest()


def input_presentation(tid: str, mid: str, payload: dict, created: float) -> dict:
    source = payload['source']
    metadata = payload.get('metadata', {})
    name = metadata.get('sourceName') or source['id']
    mode = metadata.get('deliveryMode', 'after-turn')
    stamp = datetime.fromtimestamp(created).strftime('%m-%d %H:%M')
    title = ask_title(payload, name, stamp)
    if title is None:
        title = f'Received message from {name} · {mode} · {stamp}' if source['kind'] == 'agent' else f'{name} · {payload["eventType"]} · {stamp}'
    # UI preview is bounded independently of the full model input.
    body = payload['text'][:512]
    if len(payload['text']) > 512:
        body += '…'
    title = title.encode('utf-8')[:256].decode('utf-8', 'ignore')
    pid = 'claude-input:' + hashlib.sha256(mid.encode()).hexdigest()
    owner = metadata.get('cutexSessionId') or tid
    p = {'id': pid, 'source': source, 'title': title, 'body': body,
         'format': 'plainText', 'references': []}
    semantic = framed(b'codex:presentation:semantic:v1\0',
                      [owner, tid, pid, source['kind'], source['id'], title, body, 'plainText'])
    receipt = framed(b'codex:presentation:receipt:v1\0', [owner, tid, pid, semantic])
    return {'version': 1, 'ownerId': owner, 'originThreadId': tid, 'presentation': p,
            'semanticSha256': semantic, 'receiptId': receipt}
