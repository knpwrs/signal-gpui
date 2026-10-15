# Vendored dependencies

Copies of upstream crates with small changes this app needs, used through
`[patch]` in the workspace `Cargo.toml`. Each keeps its upstream license.

## presage

whisperfish/presage at `32ffc659fce2f0be7fb01cffdf8c3ebc0c757d80`, the
`presage` crate only (its dev-dependencies removed, as they point into the
upstream workspace). Changes:

- `src/manager/registered.rs`: the receiving websocket sends
  `X-Signal-Receive-Stories: true`, so the server delivers stories (upstream
  hard-codes `false`).

## libsignal-service

whisperfish/libsignal-service-rs at `fb92b13c82fde40ba4d10696cf79aa4f36ce25f1`
(presage's), without its examples and dev-dependencies. Unchanged so far.
