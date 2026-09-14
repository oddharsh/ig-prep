# Embedded ICC profiles

These files are unmodified copies from [Compact ICC Profiles](https://github.com/saucecontrol/Compact-ICC-Profiles), revision `bdd84663061bc4ae95ca70decff54f581e27f702`.

- `sRGB-v4.icc`: the canonical output profile and the assumed profile for untagged input.
- `DisplayP3-v4.icc`: an independent source profile for decoder interoperability tests; excluded from release binaries.

Both profiles use parametric transfer curves. They are released under CC0; the upstream license is included as `LICENSE`.

| File | SHA-256 |
| --- | --- |
| sRGB-v4.icc | c56e1685d888f5edb92fe07f2750f387f8fe8e91b32ff8fb0b56bfbbb9458353 |
| DisplayP3-v4.icc | cb51de38e482ee974c0c76b9689e16aad04bad16e226fed2f30c842d15ff3a3d |

To update, copy the upstream profile bytes from a pinned revision, update this provenance, and rerun the colour and macOS interoperability tests. Do not modify the profiles by hand.

## Verification note

During local validation, `sips --verify` reported an incorrect MD5 for both upstream profiles. The ICC digest calculation below matches both stored IDs. macOS also recognizes the profiles and passes the pixel-conversion tests. The upstream files remain unmodified.

```python
from pathlib import Path
import hashlib

for path in Path("profiles").glob("*.icc"):
    data = bytearray(path.read_bytes())
    stored_id = data[84:100]
    for start, end in [(44, 48), (64, 68), (84, 100)]:
        data[start:end] = bytes(end - start)
    assert hashlib.md5(data).digest() == stored_id, path
```
