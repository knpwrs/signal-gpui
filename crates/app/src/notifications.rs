//! System notifications for new messages, with an inline reply field and
//! "Mark as Read" (Signal Desktop's notifications have neither action).
//!
//! macOS only allows notifications from an app bundle: `cargo run` wraps
//! the binary in one (scripts/cargo-runner.sh), and scripts/bundle.sh makes
//! one for release builds. A bare binary simply doesn't notify.

use futures::channel::mpsc;
use signal_store::ConversationId;

/// What the user did with a notification.
#[derive(Debug, Clone, PartialEq)]
pub enum NotificationAction {
    /// Clicked: show the chat.
    Open(ConversationId),
    /// Typed a reply in the notification.
    Reply(ConversationId, String),
    MarkRead(ConversationId),
}

/// What notifications show (Desktop's Settings → Notifications).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotificationSetting {
    #[default]
    NameAndMessage,
    NameOnly,
    NoNameOrMessage,
    Off,
}

impl NotificationSetting {
    pub const KEY: &str = "notifications.content";

    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("name") => Self::NameOnly,
            Some("none") => Self::NoNameOrMessage,
            Some("off") => Self::Off,
            _ => Self::NameAndMessage,
        }
    }

    pub fn as_setting(self) -> &'static str {
        match self {
            Self::NameAndMessage => "message",
            Self::NameOnly => "name",
            Self::NoNameOrMessage => "none",
            Self::Off => "off",
        }
    }

    /// Title and body for a message from `sender` (in `group`, if any).
    pub fn text(self, sender: &str, group: Option<&str>, preview: &str) -> (String, String) {
        let name = match group {
            Some(group) => format!("{sender} in {group}"),
            None => sender.to_owned(),
        };
        match self {
            Self::NameAndMessage => (name, preview.to_owned()),
            Self::NameOnly => (name, "New Message".into()),
            Self::NoNameOrMessage | Self::Off => ("Signal".into(), "New Message".into()),
        }
    }
}

/// One notification to show.
pub struct Notification<'a> {
    /// Unique per message, so it can be removed once read.
    pub id: &'a str,
    pub conversation: &'a ConversationId,
    pub title: &'a str,
    pub subtitle: Option<&'a str>,
    pub body: &'a str,
    /// With the system's sound (Desktop's "Push notification sounds").
    pub sound: bool,
}

#[cfg(target_os = "macos")]
pub use macos::Notifier;

#[cfg(not(target_os = "macos"))]
pub struct Notifier;

#[cfg(not(target_os = "macos"))]
impl Notifier {
    pub fn start(_actions: mpsc::UnboundedSender<NotificationAction>) -> Option<Self> {
        None
    }
    pub fn post(&self, _notification: &Notification) {}
    pub fn remove(&self, _ids: &[String]) {}
    pub fn set_badge(&self, _count: usize) {}
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::OnceLock;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{AllocAnyThread, define_class, msg_send};
    use objc2_foundation::{NSArray, NSBundle, NSError, NSSet, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
        UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse, UNNotificationSound,
        UNTextInputNotificationAction, UNTextInputNotificationResponse, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    };

    use super::*;

    const CATEGORY: &str = "message";
    const REPLY: &str = "reply";
    const MARK_READ: &str = "mark-read";

    /// Where the delegate (called by the system) sends what was done.
    static ACTIONS: OnceLock<mpsc::UnboundedSender<NotificationAction>> = OnceLock::new();

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "ZiggyNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            // While the app is open (on another chat), still show it.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                // (A notification without a sound plays none.)
                handler.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                handler: &block2::DynBlock<dyn Fn()>,
            ) {
                let conversation =
                    ConversationId(response.notification().request().content().threadIdentifier().to_string());
                let action = match response.actionIdentifier().to_string().as_str() {
                    REPLY => {
                        let object: &AnyObject = response.as_ref();
                        let text = object
                            .downcast_ref::<UNTextInputNotificationResponse>()
                            .map(|r| r.userText().to_string())
                            .unwrap_or_default();
                        (!text.trim().is_empty()).then_some(NotificationAction::Reply(conversation, text))
                    }
                    MARK_READ => Some(NotificationAction::MarkRead(conversation)),
                    // Clicked (or "Show"); dismissing does nothing.
                    id if id == unsafe { objc2_user_notifications::UNNotificationDefaultActionIdentifier }.to_string() => {
                        Some(NotificationAction::Open(conversation))
                    }
                    _ => None,
                };
                if let (Some(action), Some(actions)) = (action, ACTIONS.get()) {
                    let _ = actions.unbounded_send(action);
                }
                handler.call(());
            }
        }
    );

    impl Delegate {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    pub struct Notifier {
        center: Retained<UNUserNotificationCenter>,
    }

    impl Notifier {
        /// Asks for permission and sets up the reply action. `None` outside
        /// an app bundle, where the system refuses notifications.
        pub fn start(actions: mpsc::UnboundedSender<NotificationAction>) -> Option<Self> {
            NSBundle::mainBundle().bundleIdentifier()?;
            let _ = ACTIONS.set(actions);
            let center = UNUserNotificationCenter::currentNotificationCenter();

            let delegate = Delegate::new();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // The center holds its delegate weakly; this one lives as long as the app.
            std::mem::forget(delegate);

            let reply = UNTextInputNotificationAction::actionWithIdentifier_title_options_textInputButtonTitle_textInputPlaceholder(
                &NSString::from_str(REPLY),
                &NSString::from_str("Reply"),
                UNNotificationActionOptions::empty(),
                &NSString::from_str("Send"),
                &NSString::from_str("Message"),
            );
            let mark_read = UNNotificationAction::actionWithIdentifier_title_options(
                &NSString::from_str(MARK_READ),
                &NSString::from_str("Mark as Read"),
                UNNotificationActionOptions::empty(),
            );
            let actions = NSArray::from_retained_slice(&[Retained::into_super(reply), mark_read]);
            let category = UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                &NSString::from_str(CATEGORY),
                &actions,
                &NSArray::new(),
                UNNotificationCategoryOptions::empty(),
            );
            center.setNotificationCategories(&NSSet::from_retained_slice(&[category]));

            let granted = RcBlock::new(|granted: Bool, _error: *mut NSError| {
                tracing::info!(granted = granted.as_bool(), "notification permission");
            });
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge,
                &granted,
            );
            Some(Self { center })
        }

        pub fn post(&self, notification: &Notification) {
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(notification.title));
            if let Some(subtitle) = notification.subtitle {
                content.setSubtitle(&NSString::from_str(subtitle));
            }
            content.setBody(&NSString::from_str(notification.body));
            // Groups a chat's notifications, and tells actions which chat.
            content.setThreadIdentifier(&NSString::from_str(&notification.conversation.0));
            content.setCategoryIdentifier(&NSString::from_str(CATEGORY));
            if notification.sound {
                content.setSound(Some(&UNNotificationSound::defaultSound()));
            }
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                &NSString::from_str(notification.id),
                &content,
                None,
            );
            self.center.addNotificationRequest_withCompletionHandler(&request, None);
        }

        /// Takes notifications for messages that have been read off screen.
        pub fn remove(&self, ids: &[String]) {
            if ids.is_empty() {
                return;
            }
            let ids: Vec<Retained<NSString>> = ids.iter().map(|id| NSString::from_str(id)).collect();
            self.center.removeDeliveredNotificationsWithIdentifiers(&NSArray::from_retained_slice(&ids));
        }

        /// The unread count on the Dock icon.
        pub fn set_badge(&self, count: usize) {
            self.center.setBadgeCount_withCompletionHandler(count as isize, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_follows_the_setting() {
        let text = |s: NotificationSetting| s.text("Alice", Some("Hikers"), "See you at 9");
        assert_eq!(text(NotificationSetting::NameAndMessage), ("Alice in Hikers".into(), "See you at 9".into()));
        assert_eq!(text(NotificationSetting::NameOnly), ("Alice in Hikers".into(), "New Message".into()));
        assert_eq!(text(NotificationSetting::NoNameOrMessage), ("Signal".into(), "New Message".into()));
        for s in [NotificationSetting::NameOnly, NotificationSetting::NoNameOrMessage, NotificationSetting::Off] {
            assert_eq!(NotificationSetting::from_setting(Some(s.as_setting())), s);
        }
    }
}
