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
| `adminDelete.receive` / `.send` | Group admins deleting anyone's message for everyone. Desktop turns receiving and sending on separately (`isAdminDeleteReceiveEnabled` / `isAdminDeleteSendEnabled`). | Done, both always on. People whose clients don't know `adminDelete` yet still see the message. |
| `stickerReply.send` | Sending a sticker as a reply (with a quote). | Done, always on. GIFs sent while replying still drop the reply. |
| `pollSend1to1` | Creating polls in 1:1 chats (groups already have polls). | Polls aren't supported: received ones show only as "📊 Poll: question". |
| `groupMemberLabels.edit` | Editing your own member label in a group. | Done (Group settings), always on: whether you may set one follows the group's label access, as Desktop's `getCanAddLabel`. |
| `groupTerminate.send` | Admins ending a group for everyone. | Not done. We also don't know when a group was ended, so we'd still let you send to it. Desktop blocks sends and delete-for-everyone in ended groups. |
| `binaryServiceId` | Sending service ids in the binary proto fields (`*Binary`) instead of strings. | We accept both and send strings, which is still valid. |
| `retireAccessKeyGroupSend` | Group sends with group send endorsements instead of access keys. | Up to libsignal-service-rs / presage. |
| `sendMessageViaLibsignal` | Sending through libsignal's network stack. | Not applicable. |
| `keyTransparency` | Key transparency checks in safety numbers. | Not done: safety numbers are shown and can be verified, without key transparency. |
| `localBackups` | Local (on-disk) backups. | Not applicable. |
| `disappearingCalls`, `remoteMute.send` | Calls. | Not done: call notices don't follow the disappearing timer, and there's no remote mute. |

### Limits (`global.*`), with Desktop's defaults

| Key | Desktop default | Here |
|---|---|---|
| `normalDeleteMaxAgeInSeconds` | 1 day (receivers add a day of grace) | Same, hard-coded (`crates/backend/src/deletes.rs`). |
| `adminDeleteMaxAgeInSeconds` | 1 day (receivers add a day of grace) | Same, hard-coded (`deletes::ADMIN_MAX_AGE_MS`). |
| `pinnedChatLimit` | 4 | Same (`storage::MAX_PINNED`). |
| `pinnedMessageLimit` | 3 | Same (`MAX_PINNED_MESSAGES`). |
| `attachments.maxBytes` | 100 MiB | Same (`MAX_ATTACHMENT_BYTES`). |
| `attachments.maxReceiveBytes` | 1.25 × the send limit | Same, hard-coded (`MAX_RECEIVE_ATTACHMENT_BYTES`). Desktop marks an attachment too big when a download is tried; here it's judged from the size the sender gave, so the bubble says so from the start. |
| `attachments.maxAutoDownloadSizeBytes` | 200 MiB | We auto-download up to 25 MiB (`AUTO_DOWNLOAD_MAX`). Raise it, or make it a setting? |
| `textAttachmentLimitBytes` | Size of a long-text attachment | Desktop's built-in limits, hard-coded (`crates/backend/src/long_text.rs`): bodies over 2 KiB go as a `text/x-signal-plain` attachment, received ones download and replace the body, and the composer stops at 64 KiB. Not done: Desktop's "Read more" past 800 characters, its automatic retries of a failed download (here: a "Download Full Message" button), long text in group story replies we send, and dropping received messages whose inline body is over 2 KiB (Desktop's `MessageReceiver` drops them). |
| `groupsv2.maxGroupSize` / `groupSizeHardLimit` | From config | New groups don't enforce a size limit yet; the server rejects one that's too big. |
| `nicknames.min` / `.max` | From config | Not used: nicknames aren't editable. |
| `messageQueueTimeInSeconds` | 30 days (never less) | How long the server keeps undelivered messages. Not used. |
| `videoAttachments.transcodeTargetBytes`, `mediaQuality.levels`, `calling.*`, `clientExpiration`, `maxAllowedClockSkewSeconds` | n/a | Not applicable for now. |

## Attachments in transferred history

Done for the transit tier (`FilePointer.locatorInfo`: CDN number, key, and
digest or plaintext hash), which is what the phone fills for link and sync.
Attachments only in the backup (media) tier need backup-tier credentials
and aren't downloaded.

## Deletes that arrive before their message

Desktop keeps a delete for everyone (the author's or an admin's) whose
message hasn't arrived yet and applies it when the message does
(`ts/messageModifiers/Deletes.preload.ts`, `forMessage`). We drop it, so
the message shows when it comes.

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

1:1 and group voice and video calls work (see the README): `crates/calls`
drives RingRTC v2.72.1 with Signal's prebuilt WebRTC, signaling travels as
`CallMessage`s through presage, TURN relays come from
`GET /v2/calling/relays`, and group calls go through Signal's calling server
with a membership proof from `GET /v1/groups/token`. Not done yet:

- **Call links** and adhoc calls.
- **Group call history:** a group call leaves "… started a group call" in
  the chat, but no call event is synced for it, and joining or missing one
  isn't recorded the way Desktop's call history does.
- **Group call extras:** reactions, raised hands, remote mute, the speaker
  view (the grid is all there is), and paging through more participants
  than fit on screen.
- **Live testing:** written against RingRTC's API and Desktop's behavior,
  checked with unit tests and demo-mode flow tests; not yet tried between
  real devices (group calls least of all).
- **Related flags:** `disappearingCalls` (call events follow the chat's
  disappearing timer) and `remoteMute.send` (muting someone in a group call),
  listed above.
- **libsignal versions:** RingRTC pins libsignal v0.99.1 for zkgroup while
  presage uses v0.99.0; both build side by side. Keep RingRTC's pin in step
  when presage moves.

## Voice messages

Recording, the draft kept when you leave the app, and sending work. Not
checked yet: how phones and Desktop play a recording sent from here.

## The Signal chat and megaphones

Release notes, the chat's look and megaphones follow Desktop. Not checked
with a real account yet: the fetch was run against updates2.signal.org
(the ignored test `live_release_notes`), but no note has arrived in a
real chat, since the first check starts from the current Desktop
version, and the manifest lists no megaphones now, so they've only been
seen in the demo.

## Stories

Receiving, viewing, posting (text, photo, video), replies and reactions,
hiding, story privacy (My Story, custom stories, the first post asking),
stories off and view receipts for the whole account, who viewed and
replied to ours, and Desktop's tabs work. Not done yet:

- **Sender keys:** a story goes to each recipient in turn through the
  outbound queue, where Desktop sends to a list at once with sender keys.
  Posting to hundreds of connections takes a while.
- **Story privacy, account settings, views and replies** are written
  against Desktop's code and checked with unit, store and demo-mode UI
  tests; none of it has been tried with a real account yet (the Storage
  Service writes for story lists and the account record above all).
- **Story privacy leftovers:** Desktop's group story settings (a group's
  viewers, "Remove group story") and "New group story" from the send
  screen; renaming a custom story; deleting one story of ours ("Delete
  story" in My Stories), though the backend can delete one for everyone.
  Stories posted before story recipients were kept can't be deleted for
  everyone, only here.
- **Views and replies extras:** a view's time is when we got the receipt,
  not the receipt's own time; replies can't be deleted from the list, and
  a group story's replies can't be written from it.
- **Creator extras:** Desktop's text color slider, links in text stories,
  the image editor, and captions' formatting.
- **Desktop's fonts** for text stories (Inter, EB Garamond, Parisienne,
  Barlow Condensed); macOS fonts stand in.
- **Who we take stories from:** Desktop shows a person's story when it
  shares its profile with them; here, when we have a chat with them or
  they're in our contacts.
- **Calls tab:** call details, links and new calls from it.
