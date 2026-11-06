# Component channel candidate

`index.json` is the initial **unsigned, unpublished candidate** for the public
component channel. It binds Linnet's frozen canonical r3 archives; it is not
installable without `index.json.minisig` from the existing release trust key.
No production signature is included in this milestone. Do not upload this file
by itself.

Before publication, the serialized release publisher must read the existing
verified channel (if any), choose a strictly higher sequence, set the actual UTC
publication time, and run the component index parser and canonical kit verifier.
Upload each immutable `component-<name>-<version>` archive and its minisig first,
verify downloaded bytes, then publish the signed index pair under
`channel-components`. Follow the established release signer procedure; no new
trust key is introduced. An interrupted pair update leaves clients using their
last verified cache.

The canonical archives are platform-independent JavaScript, with distinct
platform asset names. Both DSH platform archives are 68,820 bytes / SHA256
`8744fcad5b964c71bd8a96b43397b06c93974464874c173f311a08118182a950`;
both Kimi archives are 45,835 bytes / SHA256
`0729fc1410b039a182c751229c40d54b29f2a62bfe13c632bd490a61388652a5`.
Private kits copy these canonical archive/signature bytes and original signed
index pair without rebuilding or repacking them.

The wire/API contract is [component-index-v1](../docs/contracts/component-index-v1.md).
