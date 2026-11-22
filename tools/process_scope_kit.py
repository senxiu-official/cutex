"""r290 P9: process evidence belongs to a home/uid, not the whole host."""
import os
from pathlib import Path


def select_rows(rows, home, owner_uid):
    target = Path(home).resolve()
    selected = []
    for row in rows:
        uid = row.get('ownerUid')
        binding = row.get('cutexHome')
        bound = Path(binding).resolve() if binding and Path(binding).is_absolute() else None
        same_home = bound is not None and (target == bound or target.is_relative_to(bound))
        # A foreign uid is independent unless readable evidence actually binds
        # it to this home (e.g. a privileged explicitly configured writer).
        if uid is not None and uid != owner_uid and row.get('filesystemUid', uid) != owner_uid and not same_home:
            continue
        name = Path((row.get('image') or row.get('name') or '').removesuffix(' (deleted)')).name.lower()
        if uid == owner_uid and bound is not None and not same_home and name in ('cutex', 'cutex-mcp'):
            continue
        selected.append(row)
    return selected


def process_rows(d):
    import kit_activation_preflight
    rows = kit_activation_preflight.process_rows(d)
    if os.name == 'nt':
        return rows
    home = d.home.resolve()
    owner_uid = home.stat().st_uid
    for row in rows:
        entry = Path('/proc') / str(row['pid'])
        for key in ('ownerUid', 'filesystemUid', 'cutexHome'):
            row.pop(key, None)
        try:
            # /proc directory ownership can become root for a non-dumpable
            # process. Its actual effective/filesystem UIDs are in status.
            uids = next(line.split()[1:] for line in (entry / 'status').read_text().splitlines()
                        if line.startswith('Uid:'))
            uid, filesystem_uid = int(uids[1]), int(uids[3])
            fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
            born = int(fields[19])
            if row.get('born') is not None and born != row['born']:
                continue
            # Never keep an environment dump: only the home binding is used.
            values = {}
            try:
                for item in (entry / 'environ').read_bytes().split(b'\0'):
                    key, separator, value = item.partition(b'=')
                    if separator and key in (b'CUTEX_HOME', b'HOME', b'CUTEX_TEST_PRIVATE_HOME'):
                        values[key] = os.fsdecode(value)
            except (OSError, ValueError):
                pass
            binding = None
            if values.get(b'CUTEX_HOME'):
                binding = values[b'CUTEX_HOME']
            elif values.get(b'HOME'):
                binding = str(Path(values[b'HOME']) / '.cutex')
            elif values.get(b'CUTEX_TEST_PRIVATE_HOME'):
                binding = str(Path(values[b'CUTEX_TEST_PRIVATE_HOME']) / '.cutex')
            if binding is not None and not Path(binding).is_absolute():
                try:
                    binding = str(Path(os.readlink(entry / 'cwd')) / binding)
                except OSError:
                    binding = None
            after = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
            after_uids = next(line.split()[1:] for line in (entry / 'status').read_text().splitlines()
                              if line.startswith('Uid:'))
            if int(after[19]) != born or (int(after_uids[1]), int(after_uids[3])) != (uid, filesystem_uid):
                continue
            row['ownerUid'] = uid
            row['filesystemUid'] = filesystem_uid
            if binding is not None:
                row['cutexHome'] = binding
        except (OSError, ValueError, IndexError, StopIteration):
            # Unknown ownership is not affirmative foreign-user evidence.
            pass
    return select_rows(rows, home, owner_uid)
