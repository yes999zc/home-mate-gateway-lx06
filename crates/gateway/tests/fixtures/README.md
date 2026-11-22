# Test-only self-signed certificate for contract tests (NOT a production secret). Safe to commit.

## G5-2 firmware contract fixtures

`test-ca.{pem,key}` + `test-direct-server.{pem,key}`: a real two-level chain
(CA signs the leaf, SAN 127.0.0.1) — the firmware's client verifies properly,
unlike the NoVerify shortcut. Test-only, safe to commit.
