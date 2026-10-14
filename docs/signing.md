# Code signing and the Keychain prompt

On macOS the database key (one random 256-bit key that encrypts both `app.db` and presage's protocol store) lives in the login keychain as the item `signal-gpui`. When a build reads it, macOS may show:

> Signal GPUI wants to access key "signal-gpui" in your keychain.

This page explains when that prompt appears and how to make it appear once instead of after every rebuild.

`SIGNAL_GPUI_DATA_DIR` profiles keep the key in a file in that directory and never touch the Keychain, so none of this applies to them.

## What the Keychain checks

A keychain item carries two lists, and an app reads the item without a prompt only when it is on both.

**The access list** names trusted apps by their *designated requirement*, a rule derived from the code signature. For an app signed with a certificate, the rule is roughly "identifier `org.signal-gpui`, signed by this certificate", so it stays the same across rebuilds as long as the certificate does. For an ad hoc or linker-signed binary, the rule is the build's hash, so every build is a new app.

**The partition list** names apps by partition ID:

| Signature | Partition ID |
|---|---|
| Apple's own software | `apple:` |
| A certificate with an Apple team ID (Apple Development, Developer ID) | `teamid:<TEAM ID>` |
| Anything else, including self-signed certificates | `cdhash:<hash of this build>` |

Choosing **Always Allow** adds the current build to both lists. With a team ID, the partition entry is `teamid:…` and covers every later build signed by the same team. Without one, the entry is that build's `cdhash:…`, and the next build fails the partition check and prompts again, even though it passes the access list.

That second case is why the self-signed certificate from `scripts/setup-dev-signing.sh` doesn't stop the prompts: it keeps the designated requirement stable, but a self-signed certificate has no team ID.

## Stopping the prompts

1. Get a free Apple Development certificate. You need an Apple ID, not a paid developer membership. In Xcode, open Settings → Accounts, add your Apple ID if it isn't listed, select it, choose Manage Certificates, then + → Apple Development.
2. Check that it is installed:

   ```sh
   security find-identity -v -p codesigning | grep "Apple Development"
   ```

3. Run the app through the bundle (next section). `scripts/bundle.sh` picks the Apple Development certificate automatically.
4. Choose **Always Allow** at the prompt one more time. The signature changed (new certificate), so this one is expected.

To confirm a build carries a team ID:

```sh
codesign -dv "target/release/Signal GPUI.app" 2>&1 | grep TeamIdentifier
```

`TeamIdentifier=not set` means the build will keep prompting.

`scripts/bundle.sh` chooses its signing identity in this order:

1. `$SIGNAL_GPUI_SIGN_IDENTITY`, if set and installed
2. the first `Apple Development:` identity
3. the self-signed `signal-gpui dev` identity
4. ad hoc

## Running the signed build

The signature that matters is on `Signal GPUI.app`, not on the binary cargo writes. The bare `target/<profile>/signal-gpui` carries only the linker's ad hoc signature, so the Keychain treats every build of it as a new app.

- `cargo run` and `cargo run --release` go through `scripts/cargo-runner.sh`, which bundles, signs and starts `target/<profile>/Signal GPUI.app`.
- For a build you start yourself, bundle it and run the path the script prints:

  ```sh
  cargo build --release
  "$(scripts/bundle.sh)"                          # target/release/signal-gpui
  "$(scripts/bundle.sh target/debug/signal-gpui)" # any other build
  ```

The bundle is also what lets macOS show notifications.

## Changing or removing certificates

Switching identities (ad hoc to self-signed, or self-signed to Apple Development) changes the designated requirement, so the first run after the switch prompts once.

To remove the self-signed certificate, delete `signal-gpui dev` from My Certificates in Keychain Access.

Don't delete the `signal-gpui` key item. Without it neither database can be decrypted, and you would have to unlink and link again.
