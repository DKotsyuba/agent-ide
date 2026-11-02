# Pinned shared Bridge producer library

These unmodified portable packages are imported from bridge-protocol/bridge-sdk
0.2.0, source `841ce5fc3c543730d0e9832d4a2066b09d8b6a0b`.
Archive SHA256: `8f4e25e527d9d9007cafb2782661387640435c47ac7692927eafdac37f8600fa`.

Bridge's qualified service composition is
`f8383ddc3c7614a262bb50216837521960c1172b`. Library import does not claim native
qualification of IDE, deployment of that service, host-observation support or
inner-call attestation. Keep vendor source unchanged; update only from a new
checked standalone artifact. Existing registry dependencies remain locked.

Local transport-fix artifact (unpublished): SHA256
`12941d1d058ccc610dc91f25a4419f3ae0d861e23aee6763e5d7544fc4535cb1`.
Based on the source above; patch SHA256
`a220cff01e68853095b0d0ab784f03013ec39e07545c8c62051395ee3afa1adc`.
Only SDK transport and its regression tests differ; package versions and protocol
are unchanged. Response reads retain their absolute deadline after peer close.
