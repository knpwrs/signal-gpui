Vendored from [`emojibase-data`](https://emojibase.dev) 17.0.0 (MIT, see `LICENSE-emojibase`):

- `emojibase-en.json` ← `en/data.json` (labels, CLDR tags, groups, skin tones, versions)
- `shortcodes-iamcal.json` ← `en/shortcodes/iamcal.json` (the shortcode set Signal Desktop uses via emoji-datasource)
- `shortcodes-github.json` ← `en/shortcodes/github.json`

`build.rs` compiles these into a static emoji table and an FST term index.
