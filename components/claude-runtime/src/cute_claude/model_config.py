"""Adapter-local model defaults; never writes the shared Claude login settings."""
from __future__ import annotations

import hashlib
import json
import os
import tempfile
import tomllib

from .errors import require, text


# Codex values; Claude has one summary style, so all but "none" mean summarized.
REASONING_SUMMARIES = ('auto', 'concise', 'detailed', 'none')


class ModelConfig:
    def __init__(self, settings):
        self.settings = settings
        self.path = settings.state_dir.resolve() / 'config.toml'

    def read(self):
        raw = self.path.read_bytes() if self.path.exists() else b''
        return tomllib.loads(raw.decode()), hashlib.sha256(raw).hexdigest()

    def model(self):
        return self.read()[0].get('model', self.settings.model)

    def effort(self):
        value = self.read()[0].get('model_reasoning_effort', self.settings.effort)
        return None if value == 'none' else value

    def reasoning_summary(self):
        """Codex `model_reasoning_summary` from config.toml, or None (unset)."""
        value = self.read()[0].get('model_reasoning_summary')
        return value if value in REASONING_SUMMARIES else None

    def thinking(self):
        """SDK `thinking` option for a new connection."""
        if self.settings.thinking == 'disabled':
            return {'type': 'disabled'}
        summary = self.reasoning_summary()
        display = self.settings.thinking_display if summary is None else ('omitted' if summary == 'none' else 'summarized')
        return {'type': 'adaptive', 'display': display}

    def write(self, params):
        require(params.get('filePath') in (None, str(self.path)), 'only adapter-local config is writable')
        current, version = self.read()
        require(params.get('expectedVersion') in (None, version), 'configuration changed; reload before saving', -32009)
        edits = params.get('edits')
        require(isinstance(edits, list) and 0 < len(edits) <= 16, 'edits must be a nonempty list')
        for edit in edits:
            require(isinstance(edit, dict), 'invalid config edit')
            require(edit.get('mergeStrategy') == 'replace', 'model settings require replace')
            key, value = edit.get('keyPath'), edit.get('value')
            require(key in {'model', 'model_reasoning_effort', 'model_reasoning_summary'}, 'only model defaults are supported by this adapter')
            if key == 'model' and value is not None:
                text(value, 'model', limit=256)
                require(not any(ord(c) < 32 for c in value), 'invalid model name')
                require(self.settings.backend == 'subscription' or value == self.settings.model, 'backend exposes only its configured model')
            if key == 'model_reasoning_effort':
                require(value in (None, 'none', 'low', 'medium', 'high', 'xhigh', 'max'), 'Unsupported Claude effort')
                if value is None:
                    value = 'none'
            if key == 'model_reasoning_summary':
                require(value is None or value in REASONING_SUMMARIES, 'Unsupported reasoning summary')
            if value is None:
                current.pop(key, None)
            else:
                current[key] = value
        raw = ''.join(f'{key} = {json.dumps(value, ensure_ascii=False)}\n' for key, value in sorted(current.items())).encode()
        fd, name = tempfile.mkstemp(prefix='.model-config-', dir=self.path.parent)
        try:
            with os.fdopen(fd, 'wb') as out:
                out.write(raw)
                out.flush()
                os.fsync(out.fileno())
            os.replace(name, self.path)
            if os.name != 'nt':
                directory = os.open(self.path.parent, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(directory)
                finally:
                    os.close(directory)
        finally:
            if os.path.exists(name):
                os.unlink(name)
        return {'status': 'ok', 'filePath': str(self.path), 'version': hashlib.sha256(raw).hexdigest()}
