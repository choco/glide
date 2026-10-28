// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Watches Dock and WindowManager notifications for expose (mission control)
//! events.

use std::cell::RefCell;
use std::future::pending;
use std::rc::Rc;

use accessibility::AXUIElement;
use anyhow::{Context, bail};
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::{NSString, ns_string};
use tracing::{debug, warn};

use crate::actor::space_manager;
use crate::sys::app::NSRunningApplicationExt;
use crate::sys::observer::Observer;

pub struct Dock {
    #[expect(dead_code)]
    observers: Vec<Observer>,
}

struct State {
    sm_tx: space_manager::Sender,
}

#[expect(non_upper_case_globals)]
mod consts {
    pub(super) const kAXExposeExit: &str = "AXExposeExit";
    pub(super) const kAXExposeShowAllWindows: &str = "AXExposeShowAllWindows";
    pub(super) const kAXExposeShowFrontWindows: &str = "AXExposeShowFrontWindows";
    pub(super) const kAXExposeShowDesktop: &str = "AXExposeShowDesktop";
}
use consts::*;

const NOTIFICATIONS: &[&str] = &[
    kAXExposeExit,
    kAXExposeShowAllWindows,
    kAXExposeShowFrontWindows,
    kAXExposeShowDesktop,
];

impl Dock {
    pub fn new(sm_tx: space_manager::Sender) -> Self {
        let state = Rc::new(RefCell::new(State { sm_tx }));
        let observers = [
            Self::observe(ns_string!("com.apple.dock"), NOTIFICATIONS, &state),
            // On macOS 27 WindowManager posts these instead of Dock. It only
            // accepts subscriptions to the notifications it lists as supported,
            // which leaves these out, but it accepts "*". VoiceOver subscribes
            // the same way.
            Self::observe(ns_string!("com.apple.WindowManager"), &["*"], &state),
        ]
        .into_iter()
        .filter_map(|observer| match observer {
            Ok(observer) => Some(observer),
            Err(e) => {
                warn!("Failed to start dock actor: {e}");
                None
            }
        })
        .collect();
        Self { observers }
    }

    fn observe(
        bundle_id: &NSString,
        notifications: &[&'static str],
        state: &Rc<RefCell<State>>,
    ) -> anyhow::Result<Observer> {
        let apps =
            NSRunningApplication::runningApplicationsWithBundleIdentifier(bundle_id).to_vec();
        let [app] = apps.as_slice() else {
            bail!(
                "Expected one running {bundle_id} instance but found {}: {apps:?}",
                apps.len()
            );
        };
        let pid = app.pid();

        let state = state.clone();
        let observer = Observer::new(pid)
            .with_context(|| format!("Creating observer for {bundle_id}"))?
            .install(move |_elem, notif| state.borrow_mut().handle_notification(notif));
        let elem = AXUIElement::application(pid);
        for notif in notifications {
            observer
                .add_notification(&elem, notif)
                .with_context(|| format!("Adding {notif} notification to {bundle_id} observer"))?;
        }

        Ok(observer)
    }

    pub async fn run(self) {
        pending().await
    }
}

impl State {
    #[tracing::instrument(skip(self))]
    fn handle_notification(&mut self, notif: &str) {
        #[expect(non_upper_case_globals)]
        match notif {
            kAXExposeShowAllWindows | kAXExposeShowFrontWindows | kAXExposeShowDesktop => {
                debug!("Expose started");
                self.sm_tx.send(space_manager::Event::ExposeActive(true));
            }
            kAXExposeExit => {
                debug!("Expose ended");
                self.sm_tx.send(space_manager::Event::ExposeActive(false));
            }
            // WindowManager's "*" subscription also delivers its other
            // notifications.
            _ => {}
        }
    }
}
