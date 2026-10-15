"""Codex v2 `UserInput` items -> Claude user-message content.

Text items become the prompt text. Images (data URL, https URL or a local path
on this host) become Anthropic image content blocks; the original items are
kept for the `userMessage` display item, as Codex app-servers do. Explicitly
managed skill documents are injected with byte bounds; audio is refused.
"""
from __future__ import annotations

import base64
import binascii
from pathlib import Path

from .errors import RpcError, require, text

IMAGE_TYPES = {'image/png', 'image/jpeg', 'image/gif', 'image/webp'}
EXTENSIONS = {'.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg',
              '.gif': 'image/gif', '.webp': 'image/webp'}
MAX_IMAGE_BYTES = 5 * 1024 * 1024   # Anthropic API per-image limit
MAX_IMAGES = 20
MAX_TOTAL_IMAGE_BYTES = 20 * 1024 * 1024


def unsupported(kind: str) -> RpcError:
    return RpcError(-32602, f'{kind} input is not supported by Claude agents',
                    {'unsupported': True, 'runtime': 'claude', 'feature': f'input:{kind}'})


def _data_url(url: str) -> tuple[str, str]:
    header, _, data = url.partition(',')
    require(header.startswith('data:') and header.endswith(';base64') and data,
            'image data URLs must be base64 encoded')
    media = header[5:-7].lower()
    require(media in IMAGE_TYPES, f'unsupported image type {media[:40]}; use PNG, JPEG, GIF or WebP')
    try:
        raw = base64.b64decode(data, validate=True)
    except (binascii.Error, ValueError):
        raise RpcError(-32602, 'invalid base64 image data') from None
    require(len(raw) <= MAX_IMAGE_BYTES, 'image exceeds 5 MB; downscale it before sending')
    return media, data


def _local_image(path: str) -> tuple[str, str, int]:
    candidate = Path(path)
    require(candidate.is_absolute(), 'localImage path must be absolute')
    media = EXTENSIONS.get(candidate.suffix.lower())
    require(media is not None, 'localImage must be a PNG, JPEG, GIF or WebP file')
    try:
        info = candidate.stat()
    except OSError:
        raise RpcError(-32602, 'localImage file is not readable on the agent host') from None
    require(candidate.is_file() and info.st_size <= MAX_IMAGE_BYTES, 'localImage must be a regular file of at most 5 MB')
    raw = candidate.read_bytes()
    require(len(raw) <= MAX_IMAGE_BYTES, 'localImage grew beyond 5 MB')
    return media, base64.b64encode(raw).decode('ascii'), len(raw)


def parse_turn_input(items: list, *, images: bool, skills=None, cwd=None) -> tuple[str, list[dict], list[dict]]:
    """Returns (prompt text, image attachments, non-text display items)."""
    require(isinstance(items, list) and len(items) > 0, 'input must contain at least one item')
    texts, attachments, display, total = [], [], [], 0
    used_skills: set[str] = set()
    for item in items:
        require(isinstance(item, dict) and isinstance(item.get('type'), str), 'invalid input item')
        kind = item['type']
        if kind == 'text':
            texts.append(text(item.get('text'), 'input.text'))
        elif kind in {'image', 'localImage'}:
            if not images:
                raise unsupported('image')
            require(len(attachments) < MAX_IMAGES, f'at most {MAX_IMAGES} images per message')
            if kind == 'image':
                url = item.get('url')
                require(isinstance(url, str) and url, 'image input needs a url (fileId is not supported)')
                if url.startswith('data:'):
                    media, data = _data_url(url)
                    size = len(data) * 3 // 4
                    attachments.append({'type': 'base64', 'mediaType': media, 'data': data})
                else:
                    require(url.startswith('https://') and len(url) <= 8192, 'image URLs must be https:// or data: URLs')
                    size = 0
                    attachments.append({'type': 'url', 'url': url})
                display.append({'type': 'image', 'url': url})
            else:
                path = text(item.get('path'), 'localImage.path', limit=4096)
                media, data, size = _local_image(path)
                attachments.append({'type': 'base64', 'mediaType': media, 'data': data})
                display.append({'type': 'localImage', 'path': path})
            total += size
            require(total <= MAX_TOTAL_IMAGE_BYTES, 'images in one message exceed 20 MB')
        elif kind == 'mention':
            # App / plugin mentions annotate text already in the message.
            display.append({'type': 'mention', 'name': str(item.get('name', ''))[:256],
                            'path': str(item.get('path', ''))[:4096]})
        elif kind == 'skill':
            if skills is None:
                raise unsupported('skill')
            require(len(used_skills) < 8, 'at most 8 skills per message')
            instructions, metadata = skills.resolve(item, cwd)
            if metadata['path'] not in used_skills:
                used_skills.add(metadata['path'])
                texts.append(instructions)
                display.append(metadata)
        elif kind in {'audio', 'localAudio'}:
            raise unsupported('audio')
        else:
            raise RpcError(-32602, f'unsupported input item type {kind[:40]}')
    value = '\n'.join(texts)
    if not attachments:
        text(value, 'combined input')
    else:
        require(len(value.encode('utf-8')) <= 65536, 'combined input exceeds 65536 bytes')
    return value, attachments, display


def content_blocks(prompt: str, attachments: list[dict]) -> str | list[dict]:
    """SDK user-message content: a plain string, or text + image blocks."""
    if not attachments:
        return prompt
    blocks: list[dict] = [{'type': 'text', 'text': prompt}] if prompt else []
    for attachment in attachments:
        if attachment['type'] == 'url':
            blocks.append({'type': 'image', 'source': {'type': 'url', 'url': attachment['url']}})
        else:
            blocks.append({'type': 'image', 'source': {'type': 'base64', 'media_type': attachment['mediaType'],
                                                       'data': attachment['data']}})
    return blocks


def sdk_user_message(prompt: str, attachments: list[dict], *, verbatim: bool) -> dict:
    """One streamed SDK user message. `client_composed` (Claude Code >= 2.1.248)
    delivers the text as written: no @path expansion, no slash-command dispatch."""
    message = {'type': 'user', 'message': {'role': 'user', 'content': content_blocks(prompt, attachments)},
               'parent_tool_use_id': None}
    if verbatim:
        message['client_composed'] = True
    return message
