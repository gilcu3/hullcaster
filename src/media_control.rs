use std::{sync::mpsc::Sender, thread, time::Duration};

use souvlaki::{MediaControlEvent, MediaControls, PlatformConfig};

use crate::config::TICK_RATE;

pub enum ControlMessage {
    PlayPause,
}

pub fn init_controls(tx_to_ui: Sender<ControlMessage>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let config = PlatformConfig {
            dbus_name: "hullcaster",
            display_name: "Hullcaster",
            hwnd: None,
        };
        let mut controls = MediaControls::new(config).unwrap();

        controls
            .attach(move |event: MediaControlEvent| match event {
                MediaControlEvent::Toggle => {
                    let _ = tx_to_ui.send(ControlMessage::PlayPause);
                }
                MediaControlEvent::Next => {}
                _ => {}
            })
            .unwrap();
        let refresh_duration = Duration::from_millis(TICK_RATE);
        loop {
            // update_control_metadata(state, &mut controls, &mut info)?;
            std::thread::sleep(refresh_duration);
        }
    })
}
