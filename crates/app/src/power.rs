//! The machine waking from sleep (Desktop's `powerMonitor` `resume`,
//! `ts/main/powerChannel.main.ts`). Connections that were open when it
//! went to sleep may have died meanwhile without saying so.

/// Calls `f` each time the Mac wakes from sleep, for as long as the app
/// runs, on the thread that posts it (the main thread). Power Nap's brief dark wakes don't count, as
/// they don't for Electron's `resume`.
#[cfg(target_os = "macos")]
pub fn on_wake(f: impl Fn() + Send + 'static) {
    use block2::RcBlock;
    use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification};
    use objc2_foundation::NSNotification;

    let block = RcBlock::new(move |_: std::ptr::NonNull<NSNotification>| {
        tracing::info!("woke from sleep");
        f();
    });
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: no object to match, no queue (the posting thread), and a
    // block that only calls `f`, which is `Send`.
    let observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(Some(NSWorkspaceDidWakeNotification), None, None, &block)
    };
    // Observed until the app quits.
    std::mem::forget(observer);
}

#[cfg(not(target_os = "macos"))]
pub fn on_wake(_f: impl Fn() + Send + 'static) {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification};

    #[test]
    fn a_wake_calls_back() {
        let woke = Arc::new(AtomicUsize::new(0));
        let counter = woke.clone();
        super::on_wake(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // What macOS posts on waking, posted here (to this process only).
        unsafe { center.postNotificationName_object(NSWorkspaceDidWakeNotification, None) };
        assert_eq!(woke.load(Ordering::SeqCst), 1);
    }
}
