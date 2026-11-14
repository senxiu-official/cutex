"""Bounded, allowlisted diagnostics from official CLI error messages.

Never retain provider prose: it may contain prompts, URLs or credentials. The SDK's
server_error includes network failures and does not by itself prove HTTP 500.
"""
from dataclasses import dataclass
from email.utils import parsedate_to_datetime
import json
import math
import re
import time
from typing import Any


@dataclass(frozen=True)
class ErrorDiagnostic:
    category: str
    http_status: int | None = None
    request_id: str | None = None
    retry_after: float | None = None

    def details(self) -> dict[str, Any]:
        return {'category': self.category, 'httpStatusCode': self.http_status,
                'requestId': self.request_id}


_HTTP = re.compile(r'^(?:API Error:\s*(?:HTTP\s+)?|HTTP\s+)([45]\d{2})(?=[\s:.,]|$)', re.I)
_REPEATED_OVERLOAD = re.compile(r'^(?:API Error:\s*)?Repeated\s+529\s+Overloaded\s+errors\b', re.I)
_NETWORK = re.compile(
    r'^API Error:\s*(?:Connection (?:dropped|failed|timed out)|'
    r"Can.t reach the API server|Request timed out)[^\r\n]{0,240}"
    r'\((ECONNRESET|ENOTFOUND|ETIMEDOUT)\)', re.I)
_OVERLOADED = re.compile(r'^(?:API Error:\s*)?overloaded(?:_error)?(?=[\s:.,]|$)', re.I)
_REQUEST_ID = re.compile(r'req_[A-Za-z0-9]{16,64}', re.ASCII)


def diagnose(message: Any, texts: list[Any]) -> ErrorDiagnostic:
    # Structured IDs only; never fish IDs out of arbitrary provider prose.
    request_id = None
    for key in ('request_id', 'requestId'):
        value = message.get(key) if isinstance(message, dict) else getattr(message, key, None)
        if isinstance(value, str) and _REQUEST_ID.fullmatch(value):
            request_id = value
            break
    headers = message.get('headers') if isinstance(message, dict) else getattr(message, 'headers', None)
    retry_after = None
    if isinstance(headers, dict):
        value = headers.get('retry-after', headers.get('Retry-After'))
        if isinstance(value, str) and len(value) <= 80:
            try:
                retry_after = float(value) if re.fullmatch(r'\d+(?:\.\d+)?', value) else parsedate_to_datetime(value).timestamp() - time.time()
                if not math.isfinite(retry_after):
                    retry_after = None
                elif retry_after < 0:
                    retry_after = 0.0
            except (ValueError, TypeError, OverflowError):
                pass
    diagnostics = []
    for text in texts[:16]:
        if not isinstance(text, str):
            continue
        bounded = text[:1024].strip()
        status = _HTTP.match(bounded)
        network = _NETWORK.match(bounded)
        if status:
            code = int(status[1])
            # The CLI often embeds an API JSON error after its status prefix.
            # Only the top-level request_id is retained, with the same strict format.
            body = bounded[status.end():].lstrip()
            if request_id is None and body.startswith('{'):
                try:
                    payload = json.loads(body)
                    candidate = payload.get('request_id') if isinstance(payload, dict) else None
                    if isinstance(candidate, str) and _REQUEST_ID.fullmatch(candidate):
                        request_id = candidate
                except ValueError:
                    pass
            diagnostics.append(ErrorDiagnostic('overloaded' if code == 529 else 'http_error', code, request_id, retry_after))
        elif network:
            diagnostics.append(ErrorDiagnostic(network[1].upper(), request_id=request_id, retry_after=retry_after))
        elif _REPEATED_OVERLOAD.match(bounded):
            diagnostics.append(ErrorDiagnostic('overloaded', 529, request_id, retry_after))
        elif _OVERLOADED.match(bounded):
            diagnostics.append(ErrorDiagnostic('overloaded', request_id=request_id, retry_after=retry_after))
    # Conflicting diagnostic blocks are not proof of one specific network/status category.
    unique = set(diagnostics)
    return next(iter(unique)) if len(unique) == 1 else ErrorDiagnostic('provider_error', request_id=request_id)
