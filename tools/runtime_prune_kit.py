"""Release-kit seams for runtime_prune_protection (no service or credential discovery).

Ship this file and runtime_prune_protection.py beside the kit's deploy.py.
The kit calls invalidate before changing selection/images or starting recovery;
publish runs only after the final operation record is stable. Neither function
modifies operation evidence, stops services, or reads the management credential.
"""
from pathlib import Path

import runtime_prune_protection as protection


def output(deploy):
    home = protection.absolute(str(deploy.home), 'kit home')
    return home / 'runtime' / 'prune-protection.json'


def invalidate(deploy):
    protection.invalidate_index(output(deploy))
    return {'status': 'invalidated'}


def publish(deploy, operation):
    operation = Path(operation).resolve(strict=True)
    operations = (Path(deploy.root) / 'operations').resolve(strict=True)
    protection.require(operation.parent == operations,
                       'prune protection operation must belong to this kit')
    # selectedImage is the kit's explicit installed image (or current symlink),
    # never the new payload: recovery can have selected the previous binary.
    selected = protection.absolute(deploy.plan.get('selectedImage'), 'kit selectedImage')
    selected = selected.resolve(strict=True)
    base = protection.absolute(deploy.plan.get('installBase'), 'kit installBase')
    index = protection.export_index(operation, output(deploy), selected, base)
    return {'status': 'exported', 'paths': len(index['protectedPaths']),
            'operation': index['kitOperation'],
            'activeExecutableSha256': index['activeExecutableSha256']}
