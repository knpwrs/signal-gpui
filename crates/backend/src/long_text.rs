//! Long messages, as Signal Desktop sends and receives them
//! (`ts/util/longAttachment.std.ts`): a body over 2 KiB goes as a
//! `text/x-signal-plain` attachment holding the whole text, and the body
//! carries only its first 2 KiB. Mentions and formatting ranges are over the
//! whole text.

use unicode_segmentation::UnicodeSegmentation as _;

/// The most of a body sent inline, in UTF-8 bytes (Desktop's
/// `MAX_MESSAGE_BODY_BYTE_LENGTH`).
pub const MAX_BODY_BYTES: usize = 2 * 1024;

/// The longest text that can be sent, in UTF-8 bytes (Desktop's
/// `MAX_BODY_ATTACHMENT_BYTE_LENGTH`; its composer refuses more).
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Received long texts bigger than this aren't downloaded: the encrypted
/// size of a 64 KiB text, as Desktop's `AttachmentDownloadManager` checks
/// (`getAttachmentCiphertextSize` of `MAX_BODY_ATTACHMENT_BYTE_LENGTH`).
pub fn max_receive_bytes() -> u64 {
    ciphertext_size(MAX_TEXT_BYTES as u64)
}

/// Downloads of a long text tried on their own before it waits for a
/// click, as Desktop's attachment download job (`DEFAULT_RETRY_CONFIG` in
/// `ts/jobs/AttachmentDownloadManager.preload.ts`).
pub const MAX_ATTEMPTS: u32 = 5;

/// How long to wait after `failures` failed downloads before the next, or
/// `None` when that was the last try: 30 seconds, 5 minutes, 50 minutes,
/// then 6 hours (Desktop's `exponentialBackoffSleepTime` with a first
/// backoff of 30 seconds, a multiplier of 10 and a cap of 6 hours).
pub fn retry_delay_ms(failures: u32) -> Option<i64> {
    const FIRST_MS: f64 = 30_000.;
    const MULTIPLIER: f64 = 10.;
    const MAX_MS: f64 = 6. * 60. * 60. * 1000.;
    if failures == 0 || failures >= MAX_ATTEMPTS {
        return None;
    }
    Some(((FIRST_MS / MULTIPLIER) * MULTIPLIER.powi(failures as i32)).min(MAX_MS) as i64)
}

/// Whether `body` is too long to send inline (Desktop's `isBodyTooLong`).
pub fn is_too_long(body: &str) -> bool {
    body.len() > MAX_BODY_BYTES
}

/// The start of `body` that's sent inline: whole graphemes up to 2 KiB
/// (Desktop's `trimBody`, which slices with `unicodeSlice`). A body whose
/// first grapheme alone is too long becomes U+FFFE, as in Desktop.
pub fn trim_body(body: &str) -> String {
    let mut end = 0;
    for (at, grapheme) in body.grapheme_indices(true) {
        if at + grapheme.len() > MAX_BODY_BYTES {
            break;
        }
        end = at + grapheme.len();
    }
    if end == 0 { "\u{FFFE}".to_owned() } else { body[..end].to_owned() }
}

/// Desktop's `logPadSize`: attachments are padded to a size on a 5% scale.
pub(crate) fn log_pad(size: u64) -> u64 {
    let steps = ((size.max(1) as f64).ln() / 1.05f64.ln()).ceil();
    (1.05f64.powf(steps).floor() as u64).max(541)
}

/// Desktop's `getAttachmentCiphertextSize` for the transit tier: padded,
/// then AES-CBC with its IV and MAC. Desktop pads twice on the way
/// (`ts/util/AttachmentCrypto.std.ts`), so this does too.
pub(crate) fn ciphertext_size(plaintext: u64) -> u64 {
    let padded = log_pad(log_pad(plaintext));
    16 + (padded / 16 + 1) * 16 + 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_receive_limit_is_the_padded_ciphertext_of_64_kib() {
        // Worked out with Desktop's formulas separately.
        assert_eq!(log_pad(MAX_TEXT_BYTES as u64), 67_789);
        assert_eq!(log_pad(67_789), 67_789, "padding twice changes nothing");
        assert_eq!(max_receive_bytes(), 67_840);
        assert_eq!(log_pad(10), 541, "at least 541 bytes");
    }

    #[test]
    fn failed_downloads_are_retried_as_desktops_are() {
        let delays: Vec<_> = (0..=MAX_ATTEMPTS).map(retry_delay_ms).collect();
        let minute = 60_000;
        assert_eq!(delays, [None, Some(30_000), Some(5 * minute), Some(50 * minute), Some(360 * minute), None]);
    }

    #[test]
    fn bodies_are_trimmed_to_whole_graphemes() {
        assert!(!is_too_long(&"a".repeat(MAX_BODY_BYTES)));
        assert!(is_too_long(&"a".repeat(MAX_BODY_BYTES + 1)));
        assert_eq!(trim_body(&"a".repeat(3000)), "a".repeat(MAX_BODY_BYTES));
        // A 3-byte character that would straddle the limit is left out.
        let body = format!("{}€€", "a".repeat(MAX_BODY_BYTES - 4));
        assert_eq!(trim_body(&body), format!("{}€", "a".repeat(MAX_BODY_BYTES - 4)));
        // So is a family emoji (several code points joined), as a whole.
        let family = "👩\u{200d}👩\u{200d}👧\u{200d}👦";
        let body = format!("{}{family}", "a".repeat(MAX_BODY_BYTES - 10));
        assert_eq!(trim_body(&body), "a".repeat(MAX_BODY_BYTES - 10));
        // Nothing fits: Desktop's stand-in.
        let huge = format!("e{}", "\u{301}".repeat(2000));
        assert_eq!(trim_body(&huge), "\u{FFFE}");
    }
}
