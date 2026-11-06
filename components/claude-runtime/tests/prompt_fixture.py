"""The normal 200-line/25-KiB index limit, with non-ASCII and a final canary."""
LAST_LINE = '- [末行](last.md) — INDEX_LAST_25K_记忆尾行'
AGENTS_MARKER = 'FULL_AGENTS_INSTRUCTIONS_SENTINEL'


def memory_index():
    last = LAST_LINE + '\n'
    available = 25 * 1024 - len(last.encode())
    rows = []
    for i in range(199):
        size = available // 199 + int(i < available % 199)
        prefix = f'- [记忆{i:03}](m{i:03}.md) — '
        room = size - len(prefix.encode()) - 1
        rows.append(prefix + '中' * (room // 3) + 'x' * (room % 3) + '\n')
    text = ''.join(rows) + last
    assert len(text.encode()) == 25 * 1024 and len(text.splitlines()) == 200
    return text


def large_append():
    # The old argv representation exceeds Windows' 32767 UTF-16-unit limit,
    # while the append is below the new 64-KiB provider instruction budget.
    agents = AGENTS_MARKER + '\n'
    agents += 'policy line\n' * ((30 * 1024 - len(agents)) // 12)
    return '# AGENTS.md\n' + agents + '\n# Memory\nFull index: `MEMORY.md`\n\n' + memory_index()
