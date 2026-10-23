# Working on Ziggy

Ziggy was called Signal GPUI (the repository was knpwrs/signal-gpui); the cargo package and the `SIGNAL_GPUI_*` environment variables keep the old name.

Lessons from building this client and from its code reviews. README.md covers what the app does; this covers how to change it without repeating old mistakes.

## Signal Desktop is the reference

When behavior is in question (what a read sync marks, how a reaction finds its target, what a backup field means), read Signal Desktop's source and do what it does. Cite the file in a comment (`ts/util/safetyNumber`, `ts/messageModifiers/ReadSyncs`). Guessing from the protocol alone produced most of the real-account bugs:

- **Edited messages answer to every edit's timestamp.** Reactions, receipts and read syncs for an edited message name the latest edit, not the original. `message_edits` records each edit's timestamp and lookups match it. A transferred backup dates an edited message by its latest revision; the original date is the oldest revision's.
- **A read sync reads everything older in its chat**, and one that arrives before its message is kept and applied when the message does. Marking only the named message left chats unread.
- **Link and sync sends more than chats and messages.** The backup also lists installed sticker packs, and each attachment's transit-tier locator. Read every frame type before deciding one doesn't matter.

## presage caches things forever

presage stores the first profile, profile photo and group photo it downloads and never refreshes them. Anything that can change needs a fresh fetch: `profiles.rs` asks the server for the current avatar path, and `refresh_group_avatar` compares the group's avatar path. Name saved files after the remote path (hashed), so a changed path means a new download. Profile fetches are rate limited: check each person at most daily and cap each sync, as Desktop's routine profile refresh does.

## Database

- **Every store transaction starts with `BEGIN IMMEDIATE`** (`AppDb::write`). A deferred transaction that reads and then writes fails at once with "database is locked" when another connection committed in between; the busy timeout doesn't apply. This aborted a history import while messages were arriving.
- **Writes queue for one connection.** `AppDb` has a read pool and a writer pool of one connection; every write, a transaction or a single statement, goes through `writer`. Writes from two connections race for SQLite's lock, and the loser fails with "database is locked" after the 5-second busy timeout: a reaction, a receipt and an incoming message were lost that way. The read pool's connections are `query_only`, so a write sent to `pool` by mistake fails at once, and the store's tests run on disk with both pools to catch it.
- **Keep each write transaction short**, since everything else waits behind it: commit per item rather than holding the lock across a long loop, and make sure each statement in a loop uses an index (check with `EXPLAIN QUERY PLAN`). `rename_authors` once scanned every message twice per person, 1 to 3 seconds a scan, all in one transaction. `json_extract` lookups need an expression index.
- **Migrations are squashed into one init.** Changing the init changes its checksum, and sqlx refuses a database whose applied migration changed, so any change to the init needs a path in `adopt_squashed_history` for databases made by the previous init. Before adding a migration, check whether a JSON field with `#[serde(default)]` or a `settings` row will do: attachment locators and verified identity keys needed no migration.
- **The history import is not one transaction** and runs alongside the receive loop. Once it starts, nothing may cancel it, and a failure leaves a partial import that is marked done.

## Privacy and the user's account

- Never use the user's messages, contacts, screenshots or photos as test data or demo content. The demo's people are made up and its images are drawn in code.
- The user's real account runs from `target/release/Ziggy.app` (`target/release/Signal GPUI.app` until the user rebuilds after the rename). Don't kill it, don't build release over it, and don't touch its data directory (`~/Library/Application Support/io.github.knpwrs.Ziggy`, before that `org.signal-gpui.Signal-GPUI`) except to read logs or databases when asked. Opening a database needs the Keychain key, which prompts the user. Any `ziggy` run without `SIGNAL_GPUI_DATA_DIR`, debug included, is also the real account, and the first one moves Signal GPUI's data directory to Ziggy's (`former_name.rs`).
- Screenshot only windows of a process you started (match by PID). Never send synthetic input. A window on another Space doesn't draw; wait for it rather than activating it.
- Don't ship Signal's Giphy key.

## Testing and screenshots

- `SIGNAL_GPUI_DEMO=1` runs with seeded demo data and no network. Several UI tests read demo names and messages, so change `demo.rs` and those tests together.
- **Always pair `SIGNAL_GPUI_DEMO=1` with `SIGNAL_GPUI_DATA_DIR=<a scratch directory>`.** The demo flag changes only the backend, not where data lives. Without a data dir, any build (debug included) opens the real profile with the Keychain key and seeds demo data into it. A 20-second run did that: `seed_profiles` cleared every username in `profiles`, overwrote `account.username` and `account.username_link`, and added demo profiles, a badge and files; the window also showed real chats and photos. Before capturing anything, check the log's `data directory:` line names the scratch directory. If it doesn't, kill the process.
- Headless GPUI tests (`#[gpui_kit::test]` with the demo backend) only see backend events if the test pumps them into `AppState` as `main.rs` does.
- To preview a state you can't click to (a dialog, a loading screen), patch the demo to emit it, capture, and restore the file. Keep a backup and confirm the patch is gone afterwards.
- Optimize README images with `oxipng -o max --strip safe`.

## Reviews and commits

- Before committing, run `coderabbit review --uncommitted --include-untracked --agent` and repeat until it finds nothing. If it reports no fresh review, add `--fresh`. Fix valid findings; skip the rest with a stated reason.
- **Budget CodeRabbit reviews.** The free plan allows 3 CLI reviews per developer per hour over a rolling window: each review becomes available again an hour after it ran ([plans](https://docs.coderabbit.ai/management/plans.md), [rate limits](https://docs.coderabbit.ai/management/rate-limits.md)). A rate-limited attempt doesn't use one; its JSON error gives the wait (`metadata.waitTime`), so retry after that, in the background, rather than in a loop. Review a whole change once it's done and its tests pass, not each small step, and split it into atomic commits afterwards by staging hunks. Each round of fixes costs another review, so fix every finding of a round before re-running.
- Findings that came up again and again:
  - Don't consume queued state (an early read sync, a pending download) until the work it stands for succeeded.
  - Don't record side data for an update that didn't apply, such as an edit timestamp for an edit to a deleted message.
  - Don't decide what a file is by sniffing its bytes when the stored content type says. A file must never go through the image optimizer.
  - Bound network work per run and schedule the rest, rather than issuing hundreds of requests at once.
  - Report every reason something was skipped, not only the common one.
- Make atomic commits, one per change, and check each builds and passes on its own. Stage hunks, then build in a separate `git worktree` with its own `CARGO_TARGET_DIR`; `git stash --keep-index` conflicts on pop when a file is partly staged.
- The repository pushes as the `knpwrs` GitHub account through the `github-knpwrs` SSH host alias.

## Noise in the logs

- `could not create sync message from a group message` (libsignal-service) is logged whenever something other than a message or edit, such as a typing indicator, goes to a group. It's harmless.
- `trusting new identity` appears as sessions with contacts start after linking.

## Writing

README and comment prose follows the user's no-slop rules: no em dashes, concrete details instead of claims, and no padding. Say what was tested and what wasn't.
