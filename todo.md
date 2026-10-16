# To do

## Server-gated features (Signal remote config)

Signal Desktop turns some features on and sets some limits from the server's
remote config (`GET /v2/config`), with the keys listed in Desktop's
`ts/RemoteConfig.dom.ts`. We fetch it as Desktop does
(`crates/backend/src/remote_config.rs`): at start before the message queue,
and when the window becomes active, at most every two hours, kept in
settings with its ETag. Version flags (`desktop.<feature>.prod`) are
compared with Desktop 8.34.0 as a production build, so `.beta` keys never
apply. Parsing, flags and limits are unit tested and the demo sets a
made-up config; the fetch hasn't been run against Signal's server.

### Version flags

| Flag | What it gates in Desktop | Here |
|---|---|---|
| `adminDelete.receive` / `.send` | Group admins deleting anyone's message for everyone. | Done, following both flags. People whose clients don't know `adminDelete` yet still see the message. |
| `stickerReply.send` | Sending a sticker as a reply (with a quote). | Done, following the flag. GIFs reply too; Desktop opens a dialog to caption a GIF with the draft first (`DraftGifMessageSendModal`), and here a GIF goes at once, so the draft and the reply stay in the composer, as for a sticker. |
| `pollSend1to1` | Creating polls in 1:1 chats (groups always have polls). | Follows the flag. Received 1:1 polls show and can be voted in either way. |
| `groupMemberLabels.edit` | Editing your own member label in a group. | Done (Group settings), following the flag: whether you may set one follows the group's label access, as Desktop's `getCanAddLabel`. |
| `groupTerminate.send` | Admins ending a group for everyone. | Done (Group settings, "End group"), following the flag. A group ended by someone else (received, synced, or in a transferred backup) blocks sending and the rest, as Desktop. Ending a group on Signal's group server hasn't been tried. |
| `binaryServiceId` | Service ids in the binary proto fields (`*Binary`) instead of strings. | Done for the messages and sync messages we build: only the binary fields with the flag on, only the strings without, as Desktop's `SendMessage.preload.ts`. Sent transcripts built in libsignal-service-rs and the Storage Service records we write (contacts, story lists, pins) carry both forms, which every client reads; Desktop writes one. We accept both. |
| `retireAccessKeyGroupSend` | Group sends with group send endorsements instead of access keys. | Always on: group messages go with a sender key and a `Group-Send-Token`, and one by one without endorsements. See "Sender keys" below. |
| `sendMessageViaLibsignal` | Sending through libsignal's network stack. | Not applicable. |
| `keyTransparency` | Key transparency checks in safety numbers. | Not done: safety numbers are shown and can be verified, without key transparency. |
| `localBackups` | Local (on-disk) backups. | Not applicable. |
| `disappearingCalls` | Call notices follow the chat's disappearing timer. | Done, always on: a call's notice takes the chat's timer and counts down once it's seen and the call is over (`signal_store::call_history`). |
| `remoteMute.send` | Muting someone in a group call. | Done, always on (`Command::RemoteMute`). |

### Limits (`global.*`), with Desktop's defaults

Each falls back to Desktop's default when the key is missing or isn't a
number.

| Key | Desktop default | Here |
|---|---|---|
| `normalDeleteMaxAgeInSeconds`, `adminDeleteMaxAgeInSeconds` | 1 day (receivers add a day of grace) | Follows the config. |
| `pinnedChatLimit`, `pinnedMessageLimit` | 4, 3 | Follows the config. |
| `attachments.maxBytes`, `attachments.maxReceiveBytes` | 100 MiB, 1.25 × the send limit | Follows the config. Desktop marks an attachment too big when a download is tried; here it's judged from the size the sender gave, so the bubble says so from the start. |
| `attachments.maxAutoDownloadSizeBytes` | 200 MiB | Follows the config. |
| `textAttachmentLimitBytes` | Size of a long-text attachment | Desktop's built-in limits, hard-coded (`crates/backend/src/long_text.rs`): bodies over 2 KiB go as a `text/x-signal-plain` attachment (group story replies too), received ones download and replace the body, a failed download is retried after 30 s, 5 min, 50 min and 6 h as Desktop's download job does, received messages and edits whose inline body is over 2 KiB are dropped and logged, the composer stops at 64 KiB, and a bubble shows 800 characters with "Read more" adding 3,000. Retries and the drop were unit tested; the downloads themselves weren't tried against Signal's CDN. |
| `groupsv2.maxGroupSize` / `groupSizeHardLimit` | 151 / 1001 | Follows the config in New group and Add members, with Desktop's alerts; the backend refuses to go past the hard limit. Not tried against Signal's group server. |
| `nicknames.min` / `.max` | 3 / 32 | Username nickname limits (`ts/util/Username.dom.ts`), not contact nicknames. Read (`username_nickname_min` / `_max`) but unused: usernames can't be set here. Contact nicknames and notes use Desktop's built-in limits (`signal_store::NICKNAME_LIMITS`), which no config key sets. |
| `messageQueueTimeInSeconds` | 30 days (never less) | How long the server keeps undelivered messages. Follows the config, never under 30 days, for erasing view-once messages left unviewed (Desktop's `getMessageQueueTime`). |
| `videoAttachments.transcodeTargetBytes`, `mediaQuality.levels`, `calling.*`, `clientExpiration`, `maxAllowedClockSkewSeconds` | n/a | Not applicable for now. |

## Attachments in transferred history

Both tiers are read (`backup_media.rs`): the backup (media) tier first
when the account has paid media backups and we have the media root key,
then the transit tier. Backup thumbnails stand in for images and videos
not downloaded. Not verified against Signal's servers (no account with
media backups was used): the REST paths and JSON (`/v1/archives/auth`,
`/v1/archives/keys`, `/v1/archives`, `/v1/archives/auth/read`)
come from Desktop's earlier REST client and Signal-Server, since Desktop
now makes those calls over libsignal's gRPC. Left:

- Desktop lists the backup's media (`backupListMedia`) to learn each
  file's CDN when the backup didn't name one; we use only the remote
  config fallback (`global.backups.mediaTierFallbackCdnNumber`).
- Desktop downloads all media in the background with paid backups; we
  download photos whole and other images and videos as thumbnails only.
- Downloads aren't resumed (Desktop's `downloadPath` and range requests).
- Credentials are kept in memory per run, not across runs as Desktop does.

## Sender keys

Group messages and stories go to everyone at once with a sender key
(`sendToGroupViaSenderKey`), with Desktop's fallback to one by one. Group
messages carry a `Group-Send-Token` from the group's send endorsements,
which are fetched with the group (`GET /v2/groups/`) when they're missing,
about to expire, or don't cover a member. Each member's result is kept
(also for those it went to one by one), and a retry to the members a send
missed (poll votes) goes with the sender key as a partial send: Desktop's
`isPartialSend`, where those left out keep our key. The wire format, the
tokens and every fallback path are unit tested (against libsignal's parser and
decrypter, a made-up zkgroup server and a fake network). Not tried
against Signal's servers. Not done:

- **Retry requests:** when someone can't read a sender key message, their
  client asks us to resend. We don't handle those requests at all (no
  send log), so we don't send our key to them again either. Group messages
  use `ContentHint::Default` rather than Desktop's `Resendable`, so their
  client shows the failure instead of waiting for a resend.
- **Old sender keys** stay in presage's store: it can't delete one, so a
  reset starts a new distribution id instead of deleting the old key, as
  Desktop does.
- **Pre-key fetches** for a sender key send are made over the identified
  connection; Desktop makes them unidentified with the access key or
  endorsement.
- **Endorsements** come only from the `GET /v2/groups/` made before a send.
  Desktop also takes them from group changes and the group logs.
- **Unregistered recipients** the server names (`uuids404`) are skipped
  for that send, but not marked unregistered, and their sessions aren't
  archived as Desktop does.

## View-once messages

Receiving, opening once, our other devices' opens (kept with the early
events when they come first, so they survive a restart), sending a photo
or video view-once (the composer's "View once" toggle), erasing ours
once sent and anything left unviewed past the server's message queue
time (at least 30 days) follow Desktop. Tested with store, mapping and
demo-mode UI tests; not tried with a real account or against phones. Not
done:

- **The composer's text:** Desktop hides the text box while view-once is
  on. Here it stays, and what was typed stays in the composer after the
  photo goes, since a view-once message carries no text.
- **Opening:** the photo or video is copied to `view-once/` in the data
  directory for the viewer and deleted when the viewer closes (or at the
  next start, if the app quit with it open). The viewer's own handling
  (deleting on close) is tested; opening one from a real message is
  tested only in the demo.
- **Viewed receipts** for one we open go only with read receipts on, as
  Desktop's `sendReceipts`; not checked against a phone.

## Polls

Receiving, voting, making (in groups) and ending polls work, following
Desktop's `messageModifiers/Polls.preload.ts`, `sendPollVote` and
`PollCreateModal`, with who may vote, unread votes and ends, vote retries,
the pending spinner and the options' emoji picker. Tested with store,
mapping and history import unit tests and demo-mode UI tests; not yet
tried with a real account, against phones, or against the server's rate
limits and refusals. Left:

- **Send states for other messages:** who a message reached is kept only
  for polls (all Desktop needs it for here), and only whether it reached
  them, not per-person delivered or read.
- **Retrying a poll:** "Retry send" on a poll that reached only some
  members sends it to everyone again (they drop the repeat by its
  timestamp); Desktop sends only to the ones it missed.
- **Ending a poll:** a failed `PollTerminate` isn't retried (Desktop keeps
  `terminateSendStatus` and retries it).
- **Spinner timing:** a vote shows its new choice at once and the spinner
  after 500 ms; Desktop keeps showing the sent choice until the spinner
  appears.

## Nicknames and notes

Reading, editing and deleting a contact's nickname and note work, following
Desktop's `EditNicknameAndNoteModal`, `NotePreviewModal` and the details
panel's "Nickname" row: written to the contact's Storage Service record
(only `nickname` and `note` change; unknown fields are kept), and the
nickname is the name everywhere a contact's name shows (titles, the chat
list, mentions, message authors, notifications, search). Tested with
store, wire-format and demo-mode UI tests; not yet tried with a real
account or checked on a phone. Not done:

- **Group members' details:** Desktop's contact modal (clicking a member in
  a group) also opens the Nickname dialog. There's no contact modal here,
  so a nickname is set from the person's 1:1 Chat settings.
- **Short names:** Desktop uses the nickname's first name where it shows
  short names (`getTitle` with `isShort`). We show full names everywhere.
- **Delete menu:** Desktop puts "Delete" in a menu beside the Nickname row,
  always shown; here it's a button shown once there's a nickname or note.

## Sticker pack creator

Built (File → Create/upload sticker pack), following Desktop's
`sticker-creator/`: images processed to Desktop's rules, emoji, title,
author and cover, confirmation, upload with progress, the share link.
Tested with unit and headless UI tests against the demo backend; the
upload hasn't been tried against Signal's servers. Left:

- **Hover preview:** Desktop shows a larger preview of a sticker after
  hovering it for half a second (`ArtFrame.tsx`).
- **Drop zone text:** Desktop's empty drop zone says "Drop images here"
  while files are dragged over it; here only its border changes, since
  GPUI's drag-over hook only restyles.
- **Share buttons:** Desktop's are round brand icons; here they're
  outlined text buttons with the same links.
- **Cancel during upload:** Desktop has one, but it only goes back a
  step while the upload carries on, so it's left out.

## Calls

1:1 and group voice and video calls work (see the README): `crates/calls`
drives RingRTC v2.72.1 with Signal's prebuilt WebRTC, signaling travels as
`CallMessage`s through presage, TURN relays come from
`GET /v2/calling/relays`, and group calls go through Signal's calling server
with a membership proof from `GET /v1/groups/token`. Not done yet:

- **Call links:** making one (admin approval on, as Desktop), opening a
  `signal.link/call` URL (from a message too), renaming, approval on or
  off, deleting, joining ("Ask to join" when an admin lets people in),
  approving or denying joiners as admin, sharing (copy, or send to chats),
  the `CallLinkUpdate` sync both ways, Storage Service `CallLinkRecord`s
  read and written, and calls through links in the call history with
  `AD_HOC_CALL` call events both ways. Written against RingRTC's call link
  API and Desktop's code, checked with unit, store, mapping and demo-mode UI
  tests; none of it has reached Signal's servers yet, so these are unverified:
  - **Credentials:** Desktop 8.34 asks for call link credentials with
    `v101=true` and checks them against newer `genericServerPublicParams`
    (version byte 1) that our zkgroup 0.99 can't read. We ask without
    `v101` and use the params Desktop 7.60 shipped. Whether the server
    still issues credentials those verify is untested; once presage moves
    to libsignal 0.101 or later, switch to Desktop's current params and
    `v101=true`.
  - **Profile keys:** Desktop sends its profile key to everyone in a call
    link's call (`sendProfileKeysForAdhocCall`) so they see our name. We
    don't, so people who don't know us see "Unknown".
  - **Leftovers:** removing someone from a call (the backend has
    `Command::RemoveFromCall`; the call screen has no button), "Approve
    all" / "Deny all", Desktop's defunct-link records and its cleanup of
    deleted links after the message queue time, retrying a Storage Service
    write that failed (it's only logged), and an unseen-calls badge on the
    Calls tab (viewing it marks every call read, as Desktop does). Links are read from the
    calling server ten at a time; the rest wait for the next sync.
- **Group call history** (Desktop's `callDisposition`): Desktop peeks a
  group's call when a `groupCallUpdate` arrives and takes who started it
  from the calling server; we take the message's sender. A group call's
  notice doesn't say "The video call has ended" once it's over, since that
  needs each chat's live call state. Our own decline is recorded as
  declined (Desktop's transition allows it, but Desktop's decline leaves
  the ring's record as it was). Tested with store, mapping and demo-mode
  tests, not between real devices.
- **Group call extras:** reactions, raised hands, remote mute, the speaker
  view and the grid's pages are tested in demo mode and with unit tests,
  not in a real call. Left: Desktop's sidebar layout, its reaction bursts
  (many of the same emoji at once), the raised hands queue ("Open queue"),
  the picker's full emoji choice (it has the six quick reactions), and
  asking for each tile's video at its own size (on-screen tiles get 360p,
  the speaker 720p).
- **Live testing:** written against RingRTC's API and Desktop's behavior,
  checked with unit tests and demo-mode flow tests; not yet tried between
  real devices (group calls least of all).
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
hiding, story privacy (My Story, custom stories with renaming, group
stories with "New group story" and "Remove group story", the first post
asking), deleting our stories and group story replies, stories off and
view receipts for the whole account, who viewed and replied to ours,
taking stories only from people we share our profile with, Desktop's
fonts, text color slider and link previews in text stories, captions'
formatting, and Desktop's tabs work. Not done yet:

- **Sender keys:** stories (each list's share, and group stories) go to
  everyone at once with a sender key, as Desktop's `sendStory`. Replies to
  group stories and deletes for everyone still go one by one. See "Sender
  keys" below.
- **Not tried with a real account:** story privacy, account settings,
  views and replies, profile sharing and groups' story send modes are
  written against Desktop's code and checked with unit, store, storage
  record, backup import and demo-mode UI tests only (the Storage Service
  writes for story lists, the account record and group records above
  all). Link previews in text stories and captions' body ranges haven't
  been seen on a phone either.
- **Deleting old stories:** stories posted before story recipients were
  kept can't be deleted for everyone, only here.
- **The image editor** (Desktop's `MediaEditor`: drawing, text, stickers,
  cropping) for photo stories. Not started: what's drawn has to be
  painted into the photo's pixels before it's uploaded, and nothing here
  does that yet.
- **Caption formatting** is written with the composer's markers
  (`*bold*` and so on), not Desktop's formatting menu, and captions can't
  mention anyone.
- **Fonts for other scripts:** Desktop picks other fonts for Arabic,
  Japanese and other scripts (`getFontNameByTextScript`); those fall back
  to the system's fonts here.
- **A text story's link preview image** is sent, but not kept or shown
  here, for ours or others' (the card shows the title).
