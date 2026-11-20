//! The macOS system picker (`SCContentSharingPicker`, macOS 14). It lists no
//! source and gives no thumbnail: the user picks in the system dialog, which is
//! what ADR 0005 asks for. A pick hands back the content filter to capture.

use std::sync::Mutex;
use std::sync::mpsc::Sender;

use dispatch2::DispatchQueue;
use objc2::Message as _;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerMode,
    SCContentSharingPickerObserver, SCShareableContentStyle, SCStream,
};

use super::ShareKind;

/// What the system dialog ended with.
pub enum PickerEvent {
    Picked(PickedContent),
    Cancelled,
    Failed,
}

/// The filter the user picked. It is immutable once handed over, so it can
/// move to the capture thread.
pub struct PickedContent(pub Retained<SCContentFilter>);

// SAFETY: an `SCContentFilter` is never mutated after the picker created it.
unsafe impl Send for PickedContent {}

struct ObserverState {
    events: Mutex<Sender<PickerEvent>>,
}

impl ObserverState {
    fn tell(&self, event: PickerEvent) {
        if let Ok(events) = self.events.lock() {
            let _ = events.send(event);
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = ObserverState]
    #[name = "RustyTeamsPickerObserver"]
    struct PickerObserver;

    unsafe impl NSObjectProtocol for PickerObserver {}

    unsafe impl SCContentSharingPickerObserver for PickerObserver {
        #[unsafe(method(contentSharingPicker:didCancelForStream:))]
        fn did_cancel(&self, _picker: &SCContentSharingPicker, _stream: Option<&SCStream>) {
            self.ivars().tell(PickerEvent::Cancelled);
        }

        #[unsafe(method(contentSharingPicker:didUpdateWithFilter:forStream:))]
        fn did_update(
            &self,
            _picker: &SCContentSharingPicker,
            filter: &SCContentFilter,
            _stream: Option<&SCStream>,
        ) {
            self.ivars()
                .tell(PickerEvent::Picked(PickedContent(filter.retain())));
        }

        #[unsafe(method(contentSharingPickerStartDidFailWithError:))]
        fn did_fail(&self, _error: &NSError) {
            self.ivars().tell(PickerEvent::Failed);
        }
    }
);

impl PickerObserver {
    fn new(events: Sender<PickerEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ObserverState {
            events: Mutex::new(events),
        });
        // SAFETY: `init` of `NSObject`, on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Wrapper to move an Objective-C object to the main queue, where AppKit-backed
/// services want to be driven. The picker and its observer are only touched there.
struct ForMainQueue<T>(T);

// SAFETY: the wrapped value is used on one thread at a time, the main queue's.
unsafe impl<T> Send for ForMainQueue<T> {}

fn style_of(kind: ShareKind) -> SCShareableContentStyle {
    match kind {
        ShareKind::Window => SCShareableContentStyle::Window,
        ShareKind::Screen => SCShareableContentStyle::Display,
    }
}

fn mode_of(kind: ShareKind) -> SCContentSharingPickerMode {
    match kind {
        ShareKind::Window => SCContentSharingPickerMode::SingleWindow,
        ShareKind::Screen => SCContentSharingPickerMode::SingleDisplay,
    }
}

/// An open picker. Dropping it takes the observer off and deactivates the picker.
pub struct OpenPicker {
    observer: Option<ForMainQueue<Retained<PickerObserver>>>,
}

impl OpenPicker {
    /// Shows the system dialog for `kind`. The result arrives on `events`.
    pub fn present(kind: ShareKind, events: Sender<PickerEvent>) -> Self {
        let observer = PickerObserver::new(events);
        let for_main = ForMainQueue(observer.clone());
        DispatchQueue::main().exec_async(move || {
            let observer = for_main;
            // SAFETY: plain message sends to the shared picker, on the main queue.
            unsafe {
                let picker = SCContentSharingPicker::sharedPicker();
                let configuration = picker.defaultConfiguration();
                configuration.setAllowedPickerModes(mode_of(kind));
                picker.setDefaultConfiguration(&configuration);
                picker.addObserver(ProtocolObject::from_ref(&*observer.0));
                picker.setActive(true);
                picker.presentPickerUsingContentStyle(style_of(kind));
            }
        });
        OpenPicker {
            observer: Some(ForMainQueue(observer)),
        }
    }
}

impl Drop for OpenPicker {
    fn drop(&mut self) {
        let Some(observer) = self.observer.take() else {
            return;
        };
        DispatchQueue::main().exec_async(move || {
            let observer = observer;
            // SAFETY: plain message sends to the shared picker, on the main queue.
            unsafe {
                let picker = SCContentSharingPicker::sharedPicker();
                picker.removeObserver(ProtocolObject::from_ref(&*observer.0));
                picker.setActive(false);
            }
        });
    }
}
