# To do

## Server-gated features (Signal remote config)

Signal Desktop turns some features on and sets some limits from the server's
remote config (`GET /v2/config`, authenticated), not from its own code. The
keys are listed in Desktop's `ts/RemoteConfig.dom.ts`.

- **Version flags** (`desktop.<feature>.beta` / `.prod`): the value is the
  lowest Desktop version that gets the feature. See
  `ts/util/isFeatureEnabled.dom.ts`.
- **Scalar keys** (`global.*`): override built-in limits.

We don't read the remote config: libsignal-service-rs has no call for it, so
every value below is hard-coded to Desktop's default. A `/v2/config` fetch
(cached, refreshed now and then) would let us follow the live values instead.
We'd have to pick a Desktop version to compare the flags against, or treat a
flag as on whenever it's set.

### Version flags

| Flag | What it gates in Desktop | Here |
|---|---|---|
| `adminDelete.receive` / `.send` | Group admins deleting anyone's message for everyone (see below). | Not done. |
| `stickerReply.send` | Sending a sticker as a reply (with a quote). | Not done. Sticker sends drop the reply. |
| `pollSend1to1` | Creating polls in 1:1 chats (groups already have polls). | Polls aren't supported: received ones show only as "📊 Poll: question". |
| `groupMemberLabels.edit` | Editing your own member label in a group. | Done (Group settings), always on: whether you may set one follows the group's label access, as Desktop's `getCanAddLabel`. |
| `groupTerminate.send` | Admins ending a group for everyone. | Not done. We also don't know when a group was ended, so we'd still let you send to it. Desktop blocks sends and delete-for-everyone in ended groups. |
| `binaryServiceId` | Sending service ids in the binary proto fields (`*Binary`) instead of strings. | We accept both and send strings, which is still valid. |
| `retireAccessKeyGroupSend` | Group sends with group send endorsements instead of access keys. | Up to libsignal-service-rs / presage. |
| `sendMessageViaLibsignal` | Sending through libsignal's network stack. | Not applicable. |
| `keyTransparency` | Key transparency checks in safety numbers. | Not done: safety numbers are shown and can be verified, without key transparency. |
| `localBackups` | Local (on-disk) backups. | Not applicable. |
| `disappearingCalls`, `remoteMute.send` | Calls. | Not applicable: no calls. |

### Limits (`global.*`), with Desktop's defaults

| Key | Desktop default | Here |
|---|---|---|
| `normalDeleteMaxAgeInSeconds` | 1 day (receivers add a day of grace) | Same, hard-coded (`crates/backend/src/deletes.rs`). |
| `adminDeleteMaxAgeInSeconds` | 1 day (plus a day of grace) | Not used until admin delete exists. |
| `pinnedChatLimit` | 4 | Same (`storage::MAX_PINNED`). |
| `pinnedMessageLimit` | 3 | Same (`MAX_PINNED_MESSAGES`). |
| `attachments.maxBytes` | 100 MiB | Same (`MAX_ATTACHMENT_BYTES`). |
| `attachments.maxReceiveBytes` | 1.25 × the send limit | Not enforced on download. |
| `attachments.maxAutoDownloadSizeBytes` | 200 MiB | We auto-download up to 25 MiB (`AUTO_DOWNLOAD_MAX`). Raise it, or make it a setting? |
| `textAttachmentLimitBytes` | Size of a long-text attachment | Long text isn't handled. Desktop sends bodies over 2 KiB as a `text/x-signal-plain` attachment and shows received ones in full. We send the whole body inline, and we show a received long message cut off. Worth fixing on its own. |
| `groupsv2.maxGroupSize` / `groupSizeHardLimit` | From config | New groups don't enforce a size limit yet; the server rejects one that's too big. |
| `nicknames.min` / `.max` | From config | Not used: nicknames aren't editable. |
| `messageQueueTimeInSeconds` | 30 days (never less) | How long the server keeps undelivered messages. Not used. |
| `videoAttachments.transcodeTargetBytes`, `mediaQuality.levels`, `calling.*`, `clientExpiration`, `maxAllowedClockSkewSeconds` | n/a | Not applicable for now. |

## Attachments in transferred history

Done for the transit tier (`FilePointer.locatorInfo`: CDN number, key, and
digest or plaintext hash), which is what the phone fills for link and sync.
Attachments only in the backup (media) tier need backup-tier credentials
and aren't downloaded.

## Admin delete

The admin delete flags and limit are behind remote config, so this follows
Desktop's code paths:

- `ts/util/canDeleteForEveryone.preload.ts`
- `ts/textsecure/processDataMessage.preload.ts`
- `ts/components/DeleteMessagesModal.dom.tsx`

Group admins can delete anyone's message in a group for everyone. Our proto
already has the field: `DataMessage.adminDelete = 29`, holding
`AdminDelete { targetAuthorAciBinary, targetSentTimestamp }`.

### Receiving

- Accept it only in a group, only from someone who is an admin of that group
  when it arrives, and within `adminDeleteMaxAgeInSeconds` plus a day of
  grace.
- Don't accept it in Note to Self, in an ended group, or on an
  already-deleted message.
- This needs member roles, which we don't store: `conversations.members` is
  sorted ACIs only. Store roles from the decrypted group state.
- Show the message as deleted.

### Sending

- When we're an admin and our own normal delete doesn't apply (someone else's
  message, or ours after 24 hours), still offer "Delete for everyone" within
  the admin window.
- Confirm first, as Desktop does: "Delete for everyone?" / "As an admin,
  group members will see that you deleted this message." Desktop shows this
  only until you've seen it once.
- Send `adminDelete` with the target's author ACI (binary) and sent
  timestamp.
- Desktop prefers a normal delete for your own messages while it's still
  allowed.

### Gating

- Desktop gates sending and receiving separately
  (`isAdminDeleteSendEnabled` / `isAdminDeleteReceiveEnabled`).
- Without remote config we'd either ship it always on or behind our own
  setting. Receiving is safe to turn on once roles are stored.
- Sending is visible to other people. Clients that don't know `adminDelete`
  yet won't apply it, so the message stays for them.

## Sticker pack creator

Make a sticker pack and upload it to Signal from here. Signal Desktop
used to build this in and now sends people to Signal's web sticker creator;
check that (and Desktop's old `sticker-creator/` code in git history) for the
current rules before building.

- **Pack contents:** a title, an author, a cover and the stickers, each with
  an emoji. Signal's published guidelines: stickers are PNG, APNG or WebP,
  512×512, at most 300 KB each, and at most 200 per pack (check these
  against the creator; they may have changed).
- **Picking images:** drop or choose files, crop or pad to square, and
  convert to WebP or PNG. The emoji search we already have can pick each
  sticker's emoji.
- **Upload:**
  - Generate a random 32-byte pack key.
  - Encrypt the manifest (the `Pack` proto: title, author, cover, stickers
    with id and emoji) and each image with keys derived from the pack key,
    as installed packs are decrypted now.
  - Ask the service for upload forms
    (`GET /v1/sticker/pack/form/{count}`, which also returns the pack id)
    and upload each file to the CDN.
  - libsignal-service-rs has no helper for this, so it's new code against
    the service API.
- **After upload:**
  - Install the pack (a sticker pack operation sync so your phone gets it
    too).
  - Show the share link: `https://signal.art/addstickers/#pack_id=…&pack_key=…`.
- **Privacy:** anyone with the link can install the pack, so confirm before
  uploading.

## Calls

Voice and video calls, one-to-one and group, as in Signal Desktop. Nothing
exists yet. Incoming `CallMessage`s are dropped, so a call to this device
doesn't ring and leaves nothing in the chat.

- **Media engine:** Signal calls run on [RingRTC](https://github.com/signalapp/ringrtc),
  Signal's Rust library over its fork of WebRTC. Desktop uses it through
  Node bindings. We'd link its Rust API directly. That means building
  Signal's WebRTC fork, a large native build, so check how RingRTC's own
  scripts produce prebuilt libraries first. It also has to pin to a RingRTC
  version compatible with the libsignal we use.
- **One-to-one signalling:** offers, answers, ICE candidates, hangups and busy
  signals travel as `CallMessage` content through the normal message path.
  RingRTC produces and consumes these; we'd route them between it and
  presage. TURN servers come from the chat service; check when Desktop
  fetches them.
- **Group calls:** they go through Signal's calling server (SFU), using
  group-scoped credentials. Ring and peek updates arrive as group
  `CallMessage`s (opaque). Call links are a separate, later step.
- **Devices:** microphone, camera and speaker selection, permissions (macOS
  asks for microphone and camera access, which needs an app bundle with the
  usage strings, as notifications already do), and screen sharing later.
- **Interface:**
  - Ringing with accept and decline, plus a call window: local and remote
    video, mute, camera, hang up, and a participant grid for groups.
  - Call buttons in the conversation header.
  - Call events in the timeline (missed, incoming, outgoing, with duration).
- **Sync:**
  - Answering or declining on one device stops the ringing on the others,
    via call event sync messages.
  - The call history list syncs as well.
- **Related flags:** `disappearingCalls` (call events follow the chat's
  disappearing timer) and `remoteMute.send` (muting someone in a group call),
  listed above.

A smaller first step: show incoming and missed calls in the timeline and
the chat list from `CallMessage`s and call event syncs, without answering.
That alone tells you someone called.
