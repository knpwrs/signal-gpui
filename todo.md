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
| `keyTransparency` | Key transparency checks in safety numbers. | Follows `desktop.keyTransparency.prod`. See "Key transparency" below. |
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
| `attachments.maxAutoDownloadSizeBytes` | 200 MiB | Follows the config, under Data usage's choices by kind. |
| `textAttachmentLimitBytes` | Size of a long-text attachment | Desktop's built-in limits, hard-coded (`crates/backend/src/long_text.rs`): bodies over 2 KiB go as a `text/x-signal-plain` attachment (group story replies too), received ones download and replace the body, a failed download is retried after 30 s, 5 min, 50 min and 6 h as Desktop's download job does, received messages and edits whose inline body is over 2 KiB are dropped and logged, the composer stops at 64 KiB, and a bubble shows 800 characters with "Read more" adding 3,000. Retries and the drop were unit tested; the downloads themselves weren't tried against Signal's CDN. |
| `groupsv2.maxGroupSize` / `groupSizeHardLimit` | 151 / 1001 | Follows the config in New group and Add members, with Desktop's alerts; the backend refuses to go past the hard limit. Not tried against Signal's group server. |
| `nicknames.min` / `.max` | 3 / 32 | Username nickname limits (`ts/util/Username.dom.ts`), not contact nicknames: the username editor checks them (`signal_backend::usernames::limits`). Contact nicknames and notes use Desktop's built-in limits (`signal_store::NICKNAME_LIMITS`), which no config key sets. |
| `messageQueueTimeInSeconds` | 30 days (never less) | How long the server keeps undelivered messages. Follows the config, never under 30 days, for erasing view-once messages left unviewed (Desktop's `getMessageQueueTime`). |
| `videoAttachments.transcodeTargetBytes`, `mediaQuality.levels`, `calling.*`, `clientExpiration`, `maxAllowedClockSkewSeconds` | n/a | Not applicable for now. |

## Settings and notifications

Settings follows Desktop's Preferences pages (General, Appearance, Chats,
Calls, Notifications, Privacy, Data usage, with While muted, Phone Number
and Blocked under them), its account record settings read and written as
Desktop does. Tested with unit, store, mapping and headless UI tests from
the demo; none of it has been tried with a real account. Not done, or not
checked:

- **Untried against the server:** the profile write for "Who can see my
  number" (our profile re-sent as the server has it, with
  `phoneNumberSharing`), `PUT /v2/accounts/phone_number_discoverability`,
  the import's sync requests, and the contact sync's photos.
- **Open at login** uses `SMAppService.mainAppService` and needs the app
  bundle (macOS 13 or later); only a stand-in ran in tests.
- **Zoom** scales the theme's text size (and anything in rems); sizes in
  pixels stay, where Desktop zooms the whole page.
- **Per-chat settings:** Desktop's per-chat "While muted" and unread
  reminder choices (contact and group record fields) aren't read or
  offered; the global ones apply everywhere.
- **Default timer:** Desktop also adds a notice to the open chat when it
  changes, and to a new 1:1 chat before the first message; neither is
  shown here. The timer still applies on our first message.
- **Blocked:** only people and groups we have a chat with are listed, and
  without Desktop's "Blocked on" date.
- **Sealed sender:** ours sent from this computer aren't marked (presage's
  send doesn't report it), nor messages from the link-time backup.
- **Missed call notifications:** Desktop 8.34 has none (only the incoming
  call notification), so there are none here either.
- **Not here from Desktop's pages:** language, spell check,
  text formatting, the default skin tone, chat export, the
  badge count settings, notification profiles, "Reset" notification
  settings, sent media quality, the device name, and the Account page.
- **Sounds:** only pop, whoosh and the ringtone; Desktop's hang-up and
  hand-raised sounds aren't played.

## Key transparency

Done the way Desktop does it (`ts/services/keyTransparency.preload.ts`),
with libsignal's own client and verifier (`libsignal-net-chat`'s
`keytrans::check`). Its three requests are rebuilt over presage's
unauthenticated websocket. That covers "Verify automatically" in the
safety number screen, the weekly self check (5 minutes after linking,
retried a day after a first failure), the Settings switch (synced through
`automaticKeyVerificationDisabled`), the introduction dialog, the error
dialog, and the data from transferred history.

Tested with unit tests on the request and response wire formats against
libsignal's code and test vectors, Desktop's retry, health and schedule
rules, a forged tree head and garbage responses (nothing is stored), and
the store. Demo-mode UI tests cover the button's states. Not yet run
against Signal's servers or a real account, so a check that succeeds
end to end has only been exercised by libsignal's own tests. Not done:

- **Our number changing:** Desktop forgets the number in our stored data
  when our number changes. We don't handle number changes at all.
- **Username capability:** Desktop sends our username hash only when our
  profile has the `usernameChangeSyncMessage` capability. We send it
  whenever the account record has a username.
- **Request timeout:** our websocket has none, so a key transparency
  request gives up after 30 seconds. libsignal-net uses its own timeout.
- **Key icon:** the idle button has no icon, because the icon set has no key.
- **Closing the screen:** the running check stops when the dialog's
  `on_close` fires. Whether Escape fires it wasn't checked. Either way the
  result is dropped the next time the screen opens.

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

- **Poll reach and send states:** every message now keeps each
  recipient's send state (`message_send_states`), but a poll's voters
  still come from its own reached list, filled as before.
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

- **Group members' details:** the contact modal (clicking a member's
  avatar or name in a group, or in Group settings) offers Nickname only for
  someone we already have a 1:1 chat with; Desktop offers it for anyone.
- **Short names:** Desktop uses the nickname's first name where it shows
  short names (`getTitle` with `isShort`). We show full names everywhere.
- **Delete menu:** Desktop puts "Delete" in a menu beside the Nickname row,
  always shown; here it's a button shown once there's a nickname or note.

## App chrome, keyboard and links

The menus, closing to the Dock, the window's saved size and place, the
shortcut guide and Desktop's shortcuts, the timeline's keyboard, and
Signal's links (`sgnl://`, `signal.me`, `signal.group`, call links)
follow Desktop 8.34 (`app/menu.std.ts`, `app/main.main.ts`,
`ShortcutGuide.dom.tsx`, `addGlobalKeyboardShortcuts.preload.ts`,
`Timeline.dom.tsx`, `ts/util/signalRoutes.std.ts`). Tested in demo mode
with headless windows. Not done, or not checked:

- **Untried on macOS itself:** hiding the window with the close button,
  bringing it back from the Dock, the menus as drawn, and `sgnl://` links
  opening the app (the bundle registers the scheme; nothing ran it). The
  test platform has none of these.
- **The Dock's Quit during a call** doesn't ask "Close Signal and end the
  call?" as ⌘Q does: GPUI can't cancel a quit macOS started. The call is
  hung up as the app quits, which may be too soon for the other side to
  hear it.
- **Username links** (`signal.me/#eu/`) are read with a new presage call
  (`Manager::lookup_username_link`) that hasn't run against the server.
- **Sticker pack links** (`signal.art/addstickers`, `sgnl://addstickers`)
  say installing from links isn't supported: Desktop opens the pack's
  preview to install it, and this app has no sticker pack manager.
- **Links Desktop handles that this app can't:** its notification tokens (`show-conversation`,
  `start-call-lobby`), linking a device and donations, which show
  Desktop's "didn't make sense" error.
- **Shortcuts left out**, as their features don't exist here: navigate by
  section (⌘T, Desktop's focus cycling), the chat menu (⌘⇧L; GPUI's
  dropdowns can't be opened from code), a message's context menu (⌘F12),
  the expanded composer (⌘⇧K, ⌘Enter), and a call's expanded preview
  (⇧P). ⌘N focuses the search box, where new chats start here, rather
  than Desktop's compose pane.
- **Menus left out:** Desktop's Speech submenu, Go to Release Notes and
  Join the Beta (Desktop's, not this app's), and its developer items.
  Contact Us opens this project's issues, not Signal's support form.
- **Zoom** from the View menu steps by Chromium's levels like Desktop's
  but, as Settings' zoom, scales text rather than the whole window.
- **The timeline** targets notices too, as Desktop does, but Enter does
  nothing on one. ⌘J finds the oldest unread message by counting back
  from the newest incoming ones (this app has no unread line yet), so a
  chat with more unread than are loaded targets the oldest loaded one.
  Page Up and Page Down weren't tested.

## Group administration

Removing members, admins, permissions (including announcement-only, which
blocks the composer for others with Desktop's banner), the group link
(on, off, reset, approval, copy, QR code), requests to join and
invitations, accepting or declining an invitation, asking to join and
taking it back, joining by a `signal.group` link, the group photo (in
settings and when creating a group, with a timer at creation), the notices
for invitations, requests and the link, and groups in common all follow
Desktop (`ts/groups.preload.ts`, `ts/groups/joinViaLink.preload.ts`,
`ts/models/conversations.preload.ts`).

Tested: each change's actions are built from made-up keys and decrypted the
way every client reads a change, presentations included
(`crate::group_admin` tests, with a made-up zkgroup server); notices with
mapping and notice-text tests; the state with store tests; every dialog and
the composer replacements with demo-mode UI tests. Not tried against
Signal's group server: the join info, avatar upload form and CDN post,
`?inviteLinkPassword=` changes, and whether pending members can fetch the
group (we assume they can, as Desktop does). Not done:

- **Retries:** Desktop retries a change on 409 (the group changed) for a
  minute, fetching it again; here one conflict fails with an error.
  Changes are queued one at a time per group, so ours don't conflict with
  each other.
- **Profile key credentials** aren't cached: each join, accept or add
  fetches one (Desktop keeps them until they expire).
- **Requesting and bouncing:** Desktop folds someone asking and cancelling
  over and over into one "requested and canceled N requests" notice
  (`admin-approval-bounce`); each shows here.
- **Profile sharing** isn't turned on for a group we join or accept (Desktop
  sets `profileSharing` and its Storage Service record).
- **Member labels' permission** ("Who can add member labels") isn't in
  Permissions yet; the backend keeps it (`labels_admins_only`).
- **Announcement-only groups:** Desktop also checks every member supports
  them (`canBeAnnouncementGroup`); we don't. Messages from non-admins in
  such a group are still shown.
- **Avatar picker:** a file chooser only; Desktop's has preset avatars,
  text avatars and cropping. The picked image is cropped to its middle
  square and scaled to 512 pixels, as Desktop's `processImageFile`.
- **Group updates to people outside the group** (invitees, whoever was
  removed or turned down) go to ACIs only, one by one.

## Profile, usernames and contact screens

Built after Desktop's `ProfileEditor`, `AvatarEditor`, `writeProfile`,
`username.preload.ts`, `UsernameEditor`, `UsernameLinkEditor`,
`AboutContactModal`, `ContactModal`, `ConversationHero` and `BadgeDialog`:
our name, about and emoji, photo (picked, Desktop's twelve presets, or
removed) written with the encrypted versioned profile and the avatar upload
form; usernames reserved, confirmed, deleted, their link reset and
colored, and the account record kept in step; About and the contact modal;
the hero at the start of chats; badges on avatars and their dialog.
Tested with unit tests against libsignal with made-up keys (profile
encryption and padding, request bodies, username hashes, proofs, link
encryption), storage wire-format tests, store tests, and demo-mode UI
tests. Nothing here was run against Signal's servers or a real account:
the profile write, avatar upload, username endpoints, badge image
downloads and the account record writes are written from Desktop's code
and the server's API and haven't been tried. Not done:

- **Profile key from the account record:** writes and our own profile
  fetches use the account record's profile key (Desktop's
  `mergeAccountRecord`), but presage keeps the key from linking for what
  it sends with messages; when the phone makes a new key the two differ
  until relinking. presage has no way to set it (a vendor change).
- **Preset photos:** Desktop's twelve illustrated presets are offered;
  its text presets (initials on a color, `AvatarTextEditor`) and color
  picker aren't. There's no crop or zoom: a picked image is cropped to the
  middle square, as Desktop's `processImageFile` does.
- **Username link extras:** Desktop's "Save" (the QR card as a PNG), its
  branded QR code with the Signal logo, and the "username corrupted"
  checks (`usernameIntegrity`) and their "Fix now" dialogs.
- **Username megaphone and onboarding** (`UsernameOnboardingModal`).
- **About:** "Signal Connection" doesn't open Desktop's explanation.
- **Contact modal:** "Add to another group" and the photo lightbox.
  Clicking a group member's avatar or name beside their message opens it;
  that path isn't covered by a UI test (Group settings' is).
- **Hero:** an accepted group doesn't get Desktop's "Name not verified"
  (Desktop's `isGroupNameVerified` needs who added us to be trusted); a
  request does.
- **Badges:** our own badges' visibility can't be changed.
- **Phone number sharing:** read from the account record for profile
  writes; there's no setting to change it here.

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

## Message requests

Done following Desktop: who is a request (`isConversationAccepted`: our
answer, profile sharing, a message of ours, the address book, an empty
1:1 chat, a group someone trusted added us to; an invitation until it's
taken), the bar in place of the composer with Desktop's wording and
confirmations, Accept (unblocks, shares our profile here and in Storage
Service, sends our profile key, joins an invited group, downloads what was
held), Delete, Block, Report spam and Report and block
(`POST /v1/messages/report/{aci}/{serverGuid}` for the newest three
incoming messages, with the sender's report spam token, retried as
Desktop's job queue does), the `MessageRequestResponse` sync both ways,
the notices (and those in transferred history), the hero with Safety
tips, the safety tips dialog, the name collision warnings and their review
dialogs, "Message Request" in the chat list and notifications, and holding
read receipts, typing indicators, sends, reactions, replies, calls (an
offer gets Desktop's NeedPermission hangup and a missed call), group call
rings, media, link preview images and long texts until accepted.

Tested with store, mapping, storage record, backup import and wire format
unit tests, and demo-mode UI tests (a made-up stranger with a contact's
name, and an invitation from another made-up stranger). Not tried with a
real account or against Signal's servers: the spam report (never sent from
a test), taking or declining a real group invitation on the group server,
the syncs with a phone, and the hangup to a real caller. Left:

- **Existing chats after upgrading:** a group nobody we trust is known to
  have added us to, where we never sent anything and Storage Service has
  no `whitelisted`, now shows as a request, as it would in Desktop; who
  added us is only known from group changes seen from now on.
- **Reports aren't kept across restarts:** Desktop's job queue is
  persisted; here a report still retrying is lost when the app quits.
- **Photos:** a request's photo isn't shown, but it may already have been
  downloaded (Desktop doesn't download it until accepted, and offers
  "click to view").
- **Links:** Desktop doesn't make links clickable in a request.
- **Removed contacts** (Desktop's `removalStage` and its
  "message-direct-hidden" wording), and leaving groups a blocked contact
  added us to, aren't done.
- **Review dialogs:** the "Signal Connection" line, opening a person's
  details, and Desktop's colliding avatars beside the group warning.
- **Name changes** in the group review ("Recently changed their profile
  name from ...") aren't tracked.
- **Address book:** who's in it is refreshed with each Storage Service
  sync, not on contact sync alone.

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

## Challenges, notices, collapse sets and receipts

Done following Desktop: captcha challenges on a 428 (held sends, the
dialog, `signalcaptcha://`, resend after), idle-phone alerts and the clock
skew warning; confirming before sending to someone whose verified safety
number changed; profile name, number, session refreshed, delivery issue,
unsupported message and verification notices; collapse sets; these
updates and group changes in transferred history; per-recipient send
states with delivery, read and viewed receipts, message details and
partial sends; delivery receipts sent (sealed sender, accepted chats,
batched); viewed receipts for voice messages. Tested with unit, store,
mapping and demo-mode UI tests; nothing here has been tried against the
server or a phone. Left:

- **Retry requests:** Desktop asks the sender to send again when it can't
  decrypt a message (`DecryptionErrorMessage`, `desktop.senderKey.retry`)
  and answers theirs by resending. Neither is done: a message we can't
  decrypt resets the session and shows "Chat session refreshed" (Desktop's
  fallback), and a retry request to us is only logged. So "Delivery issue"
  only comes from transferred history.
- **Safety number confirmation:** only the composer's sends ask. Desktop
  also asks before stickers, GIFs, voice messages, calls, forwards and
  stories, asks about keys changed in the last five seconds without
  approval, and has a review step past five people.
- **Clock skew:** shown, but Desktop also stops connecting until the clock
  is right.
- **Collapse sets:** each detail of a group change is a notice here, so
  each counts toward the 50 a set holds, and a change that ended the group
  keeps its other details collapsible. The call going on in a group stands
  for Desktop's era match by being the newest call notice, and sets don't
  update when a call starts or ends until the timeline changes.
- **Send states:** an unregistered member in a group is left out of the
  states (Desktop marks them failed the first time). Ours sent from this
  computer don't record which recipients got them by sealed sender.
  Messages sent before this keep their one status.
- **Viewed receipts:** only voice messages (as Desktop); Desktop's "played"
  dot on a voice message isn't shown.
- **Captcha:** the page's redirect into the app and a real 428 haven't been
  tried; challenges on sends other than our messages (receipts, syncs) are
  ignored.
- **Unsupported messages:** Desktop shows "Update Signal"; nothing here
  updates the app, so there's no button.

## Shared contacts, payments and gift badges

Shown as Desktop shows them (`EmbeddedContact` and `ContactDetail`,
`renderPayment` and `PaymentEventNotification`, `renderGiftBadge` and
`OutgoingGiftBadgeModal`), read only. Tested with mapping tests (a gift
badge's receipt is made with zkgroup's own issuance, made-up keys), store
tests and a demo-mode UI test; nothing here has met a real message. Left:

- **Transferred history:** contacts, payments and gift badges in the
  link-time backup aren't imported (`ContactMessage`, `PaymentNotification`,
  `GiftBadge` chat items are skipped as before).
- **Matching a contact's number:** only numbers written as E.164 (`+` and
  digits) find a chat; Desktop normalizes others with our region first.
  "Add to a group" from the details isn't there.
- **Early view syncs:** a gift badge viewed on the phone before its message
  arrives here stays unopened (view syncs aren't kept early).
- **The gift box:** drawn as an emoji on a red card, not Desktop's ribbon
  and bow images.
- **Badge lookup:** the subscription configuration's fetch
  (`GET /v1/subscription/configuration`) hasn't been tried against the
  server.

## Chat colors

Desktop's `ChatColorPicker` for the default (Settings, Appearance, "Chat
color") and per chat (its settings), its 22 presets, custom colors made in
the `CustomColorEditor` (solid or gradient), and our bubbles in the
timeline filled with them, gradients drawn as gradients. Like Desktop's,
they stay on this device: no Storage Service record carries them; only the
link-time history transfer brings the phone's (`defaultChatStyle`,
`customChatColors`, each chat's `style`). Tested with store, history-import
and color unit tests and a demo-mode Settings test. Left:

- **Wallpapers:** Desktop 8.34 has none (`1410-remove-wallpaper`; its backup
  import keeps wallpapers only in its tests), so none here either.
- **The gradient dial:** Desktop drags two knobs around the sample to set
  the angle (`GradientDial`); here Start and End are buttons and an angle
  slider turns it.
- **A custom color's menu** opens on right-click; Desktop opens it on a
  click of the chosen color.

## Chat folders

Desktop's chat folders: Settings, Chats, "Chat folders" (the list, the
suggested Unread, 1:1 chats and Groups, a folder's editor with its chats,
chat types, exceptions, only unread and muted switches, and the Save
changes and Delete dialogs), the folders above the chat list with unread
counts and their menu (Mark all read, mute or unmute all, Edit folder), and
a chat's "Add to folder" and "Remove from folder". Synced as Desktop syncs
them: each change writes the folder's `ChatFolderRecord` (unknown fields
kept), records read back with `mergeChatFolderRecord`'s rules, and an "All
chats" folder made after a sync that finds none; the link-time history
brings the phone's (`ChatFolder` frames). Kept in one settings row.
Tested with store, Storage Service wire-format and history-import tests
and a demo-mode Settings test; nothing was written to a real account's
Storage Service. Left:

- **Reordering:** Desktop drags folders; here a folder's right-click menu
  moves it up or down.
- **A failed upload** is reported and not retried until the folder changes
  again (Desktop keeps `storageNeedsSync` and retries).
- **Deleted folders** stay in the list (out of sight) for good; Desktop
  drops them once older than the message queue time
  (`chatFolderCleanupService`).
- **Recipients:** a contact goes in by service id only, without the e164
  Desktop also writes; one that's only a PNI or a phone number isn't kept.
- **The narrow chat list** (Desktop's folder select) and the folder's emoji
  picker for names aren't there.
- **Following the open chat:** Desktop switches to "All chats" when a chat
  outside the shown folder is opened from elsewhere; here the open chat
  just stays listed.

