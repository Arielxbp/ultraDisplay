//! System tray icon (StatusNotifierItem): left click shows the window, the menu has the
//! current status and Quit.

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Show,
    Quit,
}

pub struct Tray {
    pub status: String,
    pub streaming: bool,
    actions: async_channel::Sender<Action>,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "ultradisplay".into()
    }

    fn title(&self) -> String {
        "UltraDisplay".into()
    }

    fn icon_name(&self) -> String {
        if self.streaming { "video-display" } else { "video-display-symbolic" }.into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "UltraDisplay".into(),
            description: self.status.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.send_blocking(Action::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.status.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Show UltraDisplay".into(),
                icon_name: "video-display".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.actions.send_blocking(Action::Show);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit (stop streaming)".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.actions.send_blocking(Action::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub type Handle = ksni::blocking::Handle<Tray>;

pub fn spawn(actions: async_channel::Sender<Action>) -> Option<Handle> {
    let tray = Tray {
        status: "Waiting for a tablet".into(),
        streaming: false,
        actions,
    };
    match tray.spawn() {
        Ok(h) => Some(h),
        Err(err) => {
            log::warn!("no system tray available ({err}); closing the window will quit");
            None
        }
    }
}
