#!/usr/bin/python3
"""Optional Gadi authorized_keys forced command; install only after owner review."""

import os
import re
import sys


QSTAT = "/opt/pbs/default/bin/qstat"
ALLOWED = re.compile(
    r"(?:qstat|/opt/pbs/default/bin/qstat) -xf -F json ([0-9]{1,20}(?:\.gadi-pbs)?)"
)


def main():
    match = ALLOWED.fullmatch(os.environ.get("SSH_ORIGINAL_COMMAND", ""))
    if match is None or len(sys.argv) != 1:
        print("Only the fixed qstat query is permitted.", file=sys.stderr)
        return 126
    os.execv(QSTAT, [QSTAT, "-xf", "-F", "json", match.group(1)])


if __name__ == "__main__":
    sys.exit(main())
