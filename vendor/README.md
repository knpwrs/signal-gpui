# Vendored dependencies

Copies of upstream crates with the changes this app needs, used through
`[patch]` in the workspace `Cargo.toml`. Each keeps its upstream license
(AGPL-3.0-only, `LICENSE.md` in each directory) and its upstream copyright.
These are modified versions, as the AGPL's section 5 requires saying: the
changes are listed below, and `git log -- vendor/<crate>` dates each one.

## presage

whisperfish/presage at `32ffc659fce2f0be7fb01cffdf8c3ebc0c757d80`, the
`presage` crate only (its dev-dependencies removed, as they point into the
upstream workspace). Modified by Ken Powers for signal-gpui since
2026-10-15. Changes:

- `src/manager/registered.rs`: the receiving websocket sends
  `X-Signal-Receive-Stories: true`, so the server delivers stories (upstream
  hard-codes `false`).
- `src/manager/registered.rs`: `Manager::send_story` sends a story (or a
  reply to a group story) to one recipient through libsignal-service's
  `MessageSender::send_story`, with our profile key in it.
- `src/manager/registered.rs`: `Manager::remote_config` fetches the
  account's remote config through libsignal-service's `get_remote_config`.
- `src/manager/linking.rs`, `src/manager/registered.rs`,
  `src/manager/confirmation.rs`: `RegistrationData::read_receipts` holds
  the primary's read receipts setting from linking (not persisted, like
  `ephemeral_backup_key`), which Signal Desktop takes as its own
  (`AccountManager`'s `createAccount`).
- `src/manager/registered.rs`: `Manager::send_message_to_group_members`
  sends to a group's members, or only some of them, and returns each
  recipient's result (for Desktop's per-recipient send states and retries
  to the ones a send missed). `send_message_to_group` now calls it and
  behaves as before.
- `src/manager/registered.rs`: `Manager::send_message_to_group_with_sender_key`
  sends a group message with our sender key and the group's send
  endorsements (the caller keeps the `SenderKeyInfo`), to every member or
  only some (a partial send, as Desktop's `isPartialSend`), and returns a
  `GroupSendSummary` with each recipient's result, as
  `send_message_to_group_members` does, those it went to one by one
  included. All three share one body, and `send_message_to_group` behaves
  as before. Members we have no profile key for get a one-recipient
  `Group-Send-Token` in place of the access key.
  `Manager::send_story_with_sender_key` sends a story to many at once.
  `Manager::group_send_endorsements` fetches the group with its
  endorsements (`GET /v2/groups/`) and saves the group.
- `src/manager/{registered,linking,confirmation}.rs`: `RegistrationData`
  carries the media root backup key from linking
  (`RegistrationData::media_root_backup_key`), not persisted, like the
  ephemeral backup key; the app saves it. Upstream drops it.
- `src/manager/registered.rs`: `Manager::unauthenticated_websocket` hands
  out the unauthenticated chat websocket, so key transparency's
  `/v1/key-transparency/*` requests (which libsignal sends on libsignal-net's
  own unauthenticated connection) can go over it.

## libsignal-service

whisperfish/libsignal-service-rs at `fb92b13c82fde40ba4d10696cf79aa4f36ce25f1`
(presage's), without its examples and dev-dependencies. Modified by Ken
Powers for signal-gpui since 2026-10-15. Changes:

- `src/sender.rs`, `src/websocket/sender.rs`: `MessageSender::send_story`
  sends sealed with the story flag (`PUT /v1/messages/{id}?story=true`),
  without an access key and without falling back to an identified send,
  as libsignal-net's story sends do. Upstream can't send stories.
- `src/websocket/remote_config.rs` (new), `src/websocket/request.rs`:
  `SignalWebSocket::get_remote_config` fetches `GET /v2/config` with
  `If-None-Match`, returning the config (or none on a 304), the `ETag` and
  `X-Signal-Timestamp`, as Desktop's `getConfig`. Upstream has no call for
  it.
- `src/provisioning/mod.rs`: `NewDeviceRegistration::read_receipts` carries
  `ProvisionMessage.readReceipts` (upstream drops it).
- Sender keys (upstream only receives them). `src/sender/sender_key.rs`:
  Signal Desktop's `sendToGroupViaSenderKey` loop behind a
  `SenderKeyTransport` trait (so it's tested without a network), the
  `SenderKeyInfo` it updates, partial sends (`isPartialSend`: those left
  out keep our key), and the sealed sender v2 payload.
  `src/sender.rs`: `MessageSender::send_message_to_group_with_sender_key`
  (falls over to one by one as Desktop's `_shouldFailSend` allows, and
  reports each recipient's result with their service id) and
  `send_story_with_sender_key`; `send_message_to_group` split into
  `send_one_by_one` and `sync_group_send`; `try_send_content` sends a
  hand-built `Content` (a sender key distribution message); new
  `MessageSenderError` variants and `sender_key_may_fall_over`.
  `src/websocket/sender.rs`: `send_multi_recipient_message` and
  `multi_recipient_request` (`PUT /v1/messages/multi_recipient`, as
  libsignal-net sends it); a `group_send_token` on `UnidentifiedAccess`
  goes as `Group-Send-Token` (`src/unidentified_access.rs`).
  `src/websocket/request.rs`: a raw request body. `src/websocket/mod.rs`:
  `sender` is public. `src/cipher.rs`: `pad_message`.
  `src/push_service/mod.rs`: either list in a 409/410 body may be missing.
- Group send endorsements: `src/groups_v2/endorsements.rs` (receive,
  check, and build tokens as Desktop's `groupSendEndorsements.preload.ts`),
  `GroupsManager::fetch_group_send_endorsements` and
  `PushService::get_group_with_endorsements` (`GET /v2/groups/`, a
  `GroupResponse`).
- `src/cipher.rs`: the envelope's `report_spam_token` is kept, by server
  GUID, for the newest 256 decrypted envelopes, and taken once with
  `cipher::take_report_spam_token` (Desktop keeps it as the sender's
  `reportingToken` and sends it with a spam report). It's kept apart from
  `Metadata` because presage-store-sqlite (not vendored) lists that
  struct's fields exhaustively. Upstream drops the token.
- `src/provisioning/mod.rs`: `NewDeviceRegistration::media_root_backup_key`,
  from the provisioning message's `mediaRootBackupKey` (one that isn't 32
  bytes is dropped rather than failing the link). Upstream ignores it when
  linking.
- `src/groups_v2/mod.rs`: `BannedMember` and `PromotedMember` are exported
  with the other group types (upstream keeps them private to `model`, so a
  `Group`'s `members_banned` couldn't be built in tests).
- `protobuf/StorageService.proto`: `AccountRecord` fields 55 to 59
  (`reactionNotifications`, `notifyFor{Calls,Mentions,Replies}IfMuted`,
  `showUnreadReminders`), from Signal Desktop's `protos/SignalStorage.proto`,
  so the notification settings can be read. Writes edit the record's wire
  bytes, so fields neither proto knows are kept.
