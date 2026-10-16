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
- `src/manager/registered.rs`: `Manager::send_story` sends a story (or a
  reply to a group story) to one recipient through libsignal-service's
  `MessageSender::send_story`, with our profile key in it.
- `src/manager/registered.rs`: `Manager::remote_config` fetches the
  account's remote config through libsignal-service's `get_remote_config`.

## libsignal-service

whisperfish/libsignal-service-rs at `fb92b13c82fde40ba4d10696cf79aa4f36ce25f1`
(presage's), without its examples and dev-dependencies. Changes:

- `src/sender.rs`, `src/websocket/sender.rs`: `MessageSender::send_story`
  sends sealed with the story flag (`PUT /v1/messages/{id}?story=true`),
  without an access key and without falling back to an identified send,
  as libsignal-net's story sends do. Upstream can't send stories.
- `src/websocket/remote_config.rs` (new), `src/websocket/request.rs`:
  `SignalWebSocket::get_remote_config` fetches `GET /v2/config` with
  `If-None-Match`, returning the config (or none on a 304), the `ETag` and
  `X-Signal-Timestamp`, as Desktop's `getConfig`. Upstream has no call for
  it.
