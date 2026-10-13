//! The main window: live status, every setting, paired tablets and the log.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use ud_host::{Config, Event, Host, Placement, Transport, service};

use crate::notify;

const LOG_LINES: i32 = 3000;

#[derive(Debug, Clone, Copy)]
pub enum HostOp {
    Start,
    Stop,
    Restart,
}

pub struct App {
    pub window: adw::ApplicationWindow,
    pub stack: adw::ViewStack,
    host: Arc<Host>,
    config_path: Option<PathBuf>,
    config: RefCell<Config>,
    /// Config the listeners were started with (to know when a restart is needed).
    running_config: RefCell<Config>,
    pending_apply: RefCell<Option<glib::SourceId>>,
    /// Set while the app itself flips the "Accept tablets" switch (from host events),
    /// so the switch handler doesn't treat it as the user asking to start/stop.
    syncing: Cell<bool>,
    panel: Cell<(u32, u32)>,
    /// Last Streaming info, to redraw the Video row when only the frame rate changes.
    video: Cell<Option<(u32, u32, u32, bool)>>,

    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    accept: adw::SwitchRow,
    state_row: adw::ActionRow,
    tablet_row: adw::ActionRow,
    video_row: adw::ActionRow,
    host_row: adw::ActionRow,
    client_row: adw::ActionRow,
    listen_row: adw::ActionRow,
    scale_row: adw::ActionRow,
    tablets_group: adw::PreferencesGroup,
    tablet_rows: RefCell<Vec<adw::ActionRow>>,
    log_buffer: gtk::TextBuffer,
    log_view: gtk::TextView,
    /// One-line status for the tray.
    pub status: RefCell<String>,
    pub streaming: Cell<bool>,
}

impl App {
    pub fn new(
        app: &adw::Application,
        host: Arc<Host>,
        config_path: Option<PathBuf>,
    ) -> Rc<Self> {
        let config = host.config();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("UltraDisplay")
            .default_width(680)
            .default_height(760)
            .build();

        let row = |title: &str, subtitle: &str| {
            adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .subtitle_selectable(true)
                .build()
        };
        let this = Rc::new(Self {
            window,
            stack: adw::ViewStack::new(),
            host,
            config_path,
            running_config: RefCell::new(config.clone()),
            config: RefCell::new(config),
            pending_apply: RefCell::new(None),
            syncing: Cell::new(false),
            panel: Cell::new((2800, 1752)),
            video: Cell::new(None),
            toasts: adw::ToastOverlay::new(),
            banner: adw::Banner::builder()
                .title("The new output name takes effect after a restart")
                .button_label("Restart")
                .revealed(false)
                .build(),
            accept: adw::SwitchRow::builder()
                .title("Accept tablets")
                .subtitle("Listen for the tablet over USB and Wi-Fi")
                .build(),
            state_row: row("State", "Starting…"),
            tablet_row: row("Tablet", "—"),
            video_row: row("Video", "—"),
            host_row: row("Sent", "—"),
            client_row: row("Tablet decode", "—"),
            listen_row: row("Listening", "—"),
            scale_row: adw::ActionRow::builder().title("Scale").build(),
            tablets_group: adw::PreferencesGroup::builder()
                .title("Paired for Wi-Fi")
                .description(
                    "Tablets pair automatically when connected over USB. Forgetting one \
                     revokes its Wi-Fi access until it's connected over USB again.",
                )
                .build(),
            tablet_rows: RefCell::new(Vec::new()),
            log_buffer: gtk::TextBuffer::new(None),
            log_view: gtk::TextView::new(),
            status: RefCell::new("Starting…".into()),
            streaming: Cell::new(false),
        });
        this.build();
        this
    }

    fn build(self: &Rc<Self>) {
        let stack = &self.stack;
        stack.add_titled_with_icon(&self.status_page(), Some("status"), "Status", "video-display-symbolic");
        stack.add_titled_with_icon(&self.settings_page(), Some("settings"), "Settings", "emblem-system-symbolic");
        stack.add_titled_with_icon(&self.tablets_page(), Some("tablets"), "Tablets", "network-wireless-symbolic");
        stack.add_titled_with_icon(&self.log_page(), Some("log"), "Log", "utilities-terminal-symbolic");

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &adw::ViewSwitcher::builder()
                .stack(stack)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build(),
        ));
        let menu = gtk::gio::Menu::new();
        menu.append(Some("Quit (stop streaming)"), Some("app.quit"));
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .menu_model(&menu)
                .tooltip_text("Menu")
                .build(),
        );

        self.banner.connect_button_clicked({
            let this = self.clone();
            move |_| this.restart()
        });

        self.toasts.set_child(Some(stack));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&self.banner);
        toolbar.set_content(Some(&self.toasts));
        self.window.set_content(Some(&toolbar));
    }

    // ---------------------------------------------------------------- pages

    fn status_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();

        let control = adw::PreferencesGroup::new();
        self.accept.set_active(self.host.is_running());
        self.accept.connect_active_notify({
            let this = self.clone();
            move |row| {
                if this.syncing.get() || row.is_active() == this.host.is_running() {
                    return;
                }
                this.host_op(if row.is_active() { HostOp::Start } else { HostOp::Stop });
            }
        });
        control.add(&self.accept);
        page.add(&control);

        let tablet = adw::PreferencesGroup::builder().title("Tablet").build();
        for r in [&self.state_row, &self.tablet_row, &self.video_row, &self.host_row, &self.client_row] {
            tablet.add(r);
        }
        page.add(&tablet);

        let host = adw::PreferencesGroup::builder().title("This PC").build();
        if self.host.tcp_idle_slowdown() {
            const FIX: &str = "echo 'net.ipv4.tcp_slow_start_after_idle = 0' | sudo tee /etc/sysctl.d/90-ultradisplay.conf && sudo sysctl --system";
            let warn = adw::ActionRow::builder()
                .title("Wi-Fi will lag after every pause")
                .subtitle(
                    "Linux shrinks a connection's send window after ~200 ms idle, so the first \
                     frames after a static moment crawl over Wi-Fi. Run the copied command once \
                     in a terminal (needs sudo; persists across reboots).",
                )
                .build();
            warn.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
            let copy = gtk::Button::builder()
                .label("Copy fix")
                .valign(gtk::Align::Center)
                .build();
            copy.connect_clicked({
                let this = self.clone();
                move |_| {
                    this.window.clipboard().set_text(FIX);
                    this.toast("Command copied. Paste it into a terminal.");
                }
            });
            warn.add_suffix(&copy);
            host.add(&warn);
        }
        host.add(&self.listen_row);
        host.add(&adw::ActionRow::builder().title("Host name").subtitle(self.host.host_name()).build());
        page.add(&host);
        page
    }

    fn settings_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let cfg = self.config.borrow().clone();

        // Display
        let display = adw::PreferencesGroup::builder()
            .title("Display")
            .description("Applied live while streaming")
            .build();
        self.update_scale_subtitle(cfg.scale);
        self.scale_row.add_suffix(&self.slider(1.0, 3.0, 0.25, cfg.scale, 2, &[1.0, 1.5, 2.0, 2.5, 3.0], {
            let this = self.clone();
            move |v| {
                this.update_scale_subtitle(v);
                this.set_config(|c| c.scale = v);
            }
        }));
        display.add(&self.scale_row);

        const PLACEMENTS: [(Placement, &str); 6] = [
            (Placement::Auto, "Automatic"),
            (Placement::Left, "Left"),
            (Placement::Right, "Right"),
            (Placement::Above, "Above"),
            (Placement::Below, "Below"),
            (Placement::Manual, "Manual"),
        ];
        let placement = cfg.effective_placement();
        let position = adw::ComboRow::builder()
            .title("Position")
            .subtitle("Presets are relative to the monitor at (0, 0)")
            .model(&gtk::StringList::new(&PLACEMENTS.map(|(_, label)| label)))
            .selected(PLACEMENTS.iter().position(|(p, _)| *p == placement).unwrap_or(0) as u32)
            .build();
        let (px, py) = cfg.position.map_or((1920, 0), |[x, y]| (x, y));
        let pos_x = adw::SpinRow::with_range(-32768.0, 32767.0, 10.0);
        pos_x.set_title("X (logical pixels)");
        pos_x.set_value(px as f64);
        let pos_y = adw::SpinRow::with_range(-32768.0, 32767.0, 10.0);
        pos_y.set_title("Y (logical pixels)");
        pos_y.set_value(py as f64);
        for r in [&pos_x, &pos_y] {
            r.set_visible(placement == Placement::Manual);
        }
        let apply_position = {
            let (this, position, pos_x, pos_y) = (self.clone(), position.clone(), pos_x.clone(), pos_y.clone());
            move || {
                let placement = PLACEMENTS[position.selected() as usize].0;
                let manual = placement == Placement::Manual;
                pos_x.set_visible(manual);
                pos_y.set_visible(manual);
                let pos = manual.then(|| [pos_x.value() as i32, pos_y.value() as i32]);
                this.set_config(|c| {
                    c.placement = placement;
                    c.position = pos;
                });
            }
        };
        position.connect_selected_notify({
            let f = apply_position.clone();
            move |_| f()
        });
        pos_x.connect_value_notify({
            let f = apply_position.clone();
            move |_| f()
        });
        pos_y.connect_value_notify(move |_| apply_position());
        display.add(&position);
        display.add(&pos_x);
        display.add(&pos_y);

        let fps_values = [60u32, 90, 120];
        let max_fps = adw::ComboRow::builder()
            .title("Maximum frame rate")
            .subtitle("Applied live (capped by the tablet's panel)")
            .model(&gtk::StringList::new(&["60 Hz", "90 Hz", "120 Hz"]))
            .selected(fps_values.iter().position(|f| *f == cfg.max_fps).unwrap_or(2) as u32)
            .build();
        max_fps.connect_selected_notify({
            let this = self.clone();
            move |row| {
                let fps = fps_values[row.selected() as usize];
                this.set_config(|c| c.max_fps = fps);
            }
        });
        display.add(&max_fps);

        display.add(&self.switch(
            "Keep the output when the tablet disconnects",
            "Its windows stay on the (invisible) tablet output while it sleeps or is unplugged",
            cfg.keep_output,
            |c, v| c.keep_output = v,
        ));
        page.add(&display);

        // Video
        let video = adw::PreferencesGroup::builder()
            .title("Video quality")
            .description(
                "Maximum bitrate. Higher looks sharper in fast motion; the bitrate is lowered \
                 automatically if the link can't keep up. Applied live.",
            )
            .build();
        video.add(&self.bitrate_row(
            "Wi-Fi bitrate",
            cfg.lan_bitrate_mbps,
            |c, v| c.lan_bitrate_mbps = v,
        ));
        video.add(&self.bitrate_row("USB bitrate", cfg.bitrate_mbps, |c, v| c.bitrate_mbps = v));
        page.add(&video);

        // Connection
        let conn = adw::PreferencesGroup::builder().title("Connection").build();
        conn.set_description(Some("Applied immediately; turning one off ends a session over it"));
        conn.add(&self.switch(
            "USB",
            "Accept the tablet over the USB cable (adb forwarding is set up automatically)",
            cfg.usb,
            |c, v| c.usb = v,
        ));
        conn.add(&self.switch(
            "Wi-Fi",
            "Accept paired tablets over the network (TLS) and advertise this PC via mDNS",
            cfg.lan,
            |c, v| c.lan = v,
        ));
        page.add(&conn);

        // App
        let app = adw::PreferencesGroup::builder().title("App").build();
        let autostart = adw::SwitchRow::builder()
            .title("Start with the session")
            .subtitle("Launch in the tray when you log into “Niri (virtual outputs)”")
            .active(service::is_installed())
            .build();
        autostart.connect_active_notify({
            let this = self.clone();
            move |row| {
                let res = if row.is_active() {
                    service::install(&["--hidden"], false)
                } else {
                    service::uninstall(false)
                };
                if let Err(err) = res {
                    this.toast(&format!("Autostart: {err:#}"));
                }
            }
        });
        app.add(&autostart);
        let log_values = [0u32, 5, 30, 60, 300];
        let stats_log = adw::ComboRow::builder()
            .title("Statistics in the log")
            .subtitle("How often the fps/bitrate lines are written")
            .model(&gtk::StringList::new(&["Off", "Every 5 s", "Every 30 s", "Every minute", "Every 5 minutes"]))
            .selected(
                log_values
                    .iter()
                    .position(|v| *v >= cfg.stats_log_secs && (cfg.stats_log_secs > 0 || *v == 0))
                    .unwrap_or(log_values.len() - 1) as u32,
            )
            .build();
        stats_log.connect_selected_notify({
            let this = self.clone();
            move |row| {
                let secs = log_values[row.selected() as usize];
                this.set_config(|c| c.stats_log_secs = secs);
            }
        });
        app.add(&stats_log);
        app.add(&self.switch(
            "Notify on connect/disconnect",
            "",
            cfg.gui.notify_connection,
            |c, v| c.gui.notify_connection = v,
        ));
        app.add(&self.switch("Notify on errors", "", cfg.gui.notify_errors, |c, v| c.gui.notify_errors = v));
        app.add(&self.switch(
            "Notify when the bitrate is lowered",
            "The link couldn't keep up",
            cfg.gui.notify_bitrate,
            |c, v| c.gui.notify_bitrate = v,
        ));
        page.add(&app);

        // Advanced
        let adv = adw::PreferencesGroup::builder()
            .title("Advanced")
            .description("Ports apply immediately; the output name after a restart")
            .build();

        adv.add(&self.switch(
            "Zero-copy GPU capture",
            "Keep frames on the GPU (much lower CPU). Falls back automatically if unavailable.",
            cfg.zero_copy,
            |c, v| c.zero_copy = v,
        ));
        adv.add(&self.port_row("USB port", cfg.port, |c, v| c.port = v));
        adv.add(&self.port_row("Wi-Fi port", cfg.lan_port, |c, v| c.lan_port = v));
        let output = adw::EntryRow::builder()
            .title("Output name")
            .text(&cfg.output)
            .show_apply_button(true)
            .build();
        output.connect_apply({
            let this = self.clone();
            move |row| {
                let name = row.text().trim().to_owned();
                if !name.is_empty() {
                    this.set_config(|c| c.output = name);
                }
            }
        });
        adv.add(&output);
        page.add(&adv);
        page
    }

    fn tablets_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        page.add(&self.tablets_group);
        self.refresh_tablets();
        page
    }

    fn log_page(self: &Rc<Self>) -> gtk::Box {
        self.log_view.set_buffer(Some(&self.log_buffer));
        self.log_view.set_editable(false);
        self.log_view.set_cursor_visible(false);
        self.log_view.set_monospace(true);
        self.log_view.set_wrap_mode(gtk::WrapMode::WordChar);
        self.log_view.set_left_margin(8);
        self.log_view.set_right_margin(8);
        let scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&self.log_view).build();

        let clear = gtk::Button::with_label("Clear");
        clear.connect_clicked({
            let buf = self.log_buffer.clone();
            move |_| buf.set_text("")
        });
        let copy = gtk::Button::with_label("Copy all");
        copy.connect_clicked({
            let (buf, win) = (self.log_buffer.clone(), self.window.clone());
            move |_| {
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
                win.clipboard().set_text(&text);
            }
        });
        let bar = gtk::ActionBar::new();
        bar.pack_end(&clear);
        bar.pack_end(&copy);

        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        b.append(&scroll);
        b.append(&bar);
        b
    }

    // -------------------------------------------------------------- widgets

    fn slider(
        &self,
        min: f64,
        max: f64,
        step: f64,
        value: f64,
        digits: i32,
        marks: &[f64],
        on_change: impl Fn(f64) + 'static,
    ) -> gtk::Scale {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
        scale.set_value(value);
        scale.set_digits(digits);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Left);
        scale.set_width_request(300);
        scale.set_valign(gtk::Align::Center);
        for m in marks {
            scale.add_mark(*m, gtk::PositionType::Bottom, None);
        }
        scale.connect_value_changed(move |s| {
            // Snap to the step (sliders report fractional values while dragging).
            let v = (s.value() / step).round() * step;
            on_change(v);
        });
        scale
    }

    fn bitrate_row(
        self: &Rc<Self>,
        title: &str,
        value: u32,
        set: impl Fn(&mut Config, u32) + 'static,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder().title(title).subtitle("Mbit/s").build();
        let this = self.clone();
        row.add_suffix(&self.slider(5.0, 150.0, 5.0, value as f64, 0, &[40.0, 80.0, 120.0], move |v| {
            this.set_config(|c| set(c, v as u32));
        }));
        row
    }

    fn switch(
        self: &Rc<Self>,
        title: &str,
        subtitle: &str,
        active: bool,
        set: impl Fn(&mut Config, bool) + 'static,
    ) -> adw::SwitchRow {
        let row = adw::SwitchRow::builder().title(title).active(active).build();
        if !subtitle.is_empty() {
            row.set_subtitle(subtitle);
        }
        let this = self.clone();
        row.connect_active_notify(move |r| {
            let v = r.is_active();
            this.set_config(|c| set(c, v));
        });
        row
    }

    fn port_row(
        self: &Rc<Self>,
        title: &str,
        value: u16,
        set: impl Fn(&mut Config, u16) + 'static,
    ) -> adw::SpinRow {
        let row = adw::SpinRow::with_range(1024.0, 65535.0, 1.0);
        row.set_title(title);
        row.set_value(value as f64);
        let this = self.clone();
        row.connect_value_notify(move |r| {
            let v = r.value() as u16;
            this.set_config(|c| set(c, v));
        });
        row
    }

    fn update_scale_subtitle(&self, scale: f64) {
        let (w, h) = self.panel.get();
        self.scale_row.set_subtitle(&format!(
            "Tablet appears as {}×{} logical pixels",
            (w as f64 / scale).round(),
            (h as f64 / scale).round()
        ));
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    // ---------------------------------------------------------------- logic

    /// Updates the config now; applies it to the host and saves it after a short pause
    /// (so dragging a slider doesn't hammer niri or the disk).
    fn set_config(self: &Rc<Self>, f: impl FnOnce(&mut Config)) {
        f(&mut self.config.borrow_mut());
        if let Some(id) = self.pending_apply.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(300), move || {
            this.pending_apply.borrow_mut().take();
            this.apply_config();
        });
        *self.pending_apply.borrow_mut() = Some(id);
    }

    fn apply_config(self: &Rc<Self>) {
        let cfg = self.config.borrow().clone();
        if let Some(path) = &self.config_path
            && let Err(err) = cfg.save(path)
        {
            self.toast(&format!("Saving settings failed: {err:#}"));
        }
        // Swapping listeners joins threads: keep it off the UI thread.
        let host = self.host.clone();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let applied = cfg.clone();
            let _ = gio::spawn_blocking(move || host.update_config(applied)).await;
            let needs_restart =
                this.host.is_running() && this.running_config.borrow().restart_required(&cfg);
            this.banner.set_revealed(needs_restart);
        });
    }

    /// Starts, stops or restarts the host on a worker thread: stopping joins the session
    /// and helper threads, which must never block the UI.
    pub fn host_op(self: &Rc<Self>, op: HostOp) {
        self.accept.set_sensitive(false);
        self.banner.set_sensitive(false);
        let host = self.host.clone();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || match op {
                HostOp::Start => host.start(),
                HostOp::Stop => {
                    host.stop();
                    Ok(())
                }
                HostOp::Restart => host.restart(),
            })
            .await;
            this.accept.set_sensitive(true);
            this.banner.set_sensitive(true);
            match res {
                Ok(Ok(())) => {
                    if this.host.is_running() {
                        *this.running_config.borrow_mut() = this.host.config();
                        this.banner.set_revealed(false);
                    }
                }
                Ok(Err(err)) => {
                    log::error!("starting: {err:#}");
                    this.toast(&format!("Couldn't start: {err:#}"));
                }
                Err(_) => this.toast("Internal error while starting/stopping"),
            }
            this.sync_accept();
        });
    }

    /// Restarts the listeners, applying a still-pending settings change first.
    pub fn restart(self: &Rc<Self>) {
        if let Some(id) = self.pending_apply.borrow_mut().take() {
            id.remove();
            self.apply_config();
        }
        self.host_op(HostOp::Restart);
    }

    /// Shows the host's actual state on the switch without triggering its handler.
    fn sync_accept(&self) {
        self.syncing.set(true);
        self.accept.set_active(self.host.is_running());
        self.syncing.set(false);
    }

    fn refresh_tablets(self: &Rc<Self>) {
        for row in self.tablet_rows.borrow_mut().drain(..) {
            self.tablets_group.remove(&row);
        }
        let devices = self.host.paired_devices();
        if devices.is_empty() {
            let row = adw::ActionRow::builder()
                .title("No tablets paired yet")
                .subtitle("Connect the tablet over USB once")
                .build();
            self.tablets_group.add(&row);
            self.tablet_rows.borrow_mut().push(row);
        }
        for dev in devices {
            let row = adw::ActionRow::builder()
                .title(&dev.name)
                .subtitle(&dev.client_id)
                .build();
            let forget = gtk::Button::builder()
                .label("Forget")
                .valign(gtk::Align::Center)
                .css_classes(["destructive-action"])
                .build();
            forget.connect_clicked({
                let (this, id) = (self.clone(), dev.client_id.clone());
                move |_| {
                    if let Err(err) = this.host.forget_device(&id) {
                        this.toast(&format!("Forget failed: {err:#}"));
                    }
                    this.refresh_tablets();
                }
            });
            row.add_suffix(&forget);
            self.tablets_group.add(&row);
            self.tablet_rows.borrow_mut().push(row);
        }
    }

    pub fn append_log(&self, line: &str) {
        let buf = &self.log_buffer;
        let mut end = buf.end_iter();
        buf.insert(&mut end, line);
        buf.insert(&mut end, "\n");
        let excess = buf.line_count() - LOG_LINES;
        if excess > 0 {
            let mut start = buf.start_iter();
            let mut cut = buf.iter_at_line(excess).unwrap_or(buf.start_iter());
            buf.delete(&mut start, &mut cut);
        }
        if self.window.is_visible() {
            let mark = buf.create_mark(None, &buf.end_iter(), false);
            self.log_view.scroll_mark_onscreen(&mark);
            buf.delete_mark(&mark);
        }
    }

    pub fn handle(self: &Rc<Self>, event: Event) {
        let prefs = self.config.borrow().gui.clone();
        let transport_name = |t: Transport| match t {
            Transport::Usb => "USB",
            Transport::Lan => "Wi-Fi",
        };
        match event {
            Event::Listening { usb_port, lan_port } => {
                let usb = usb_port.map_or("USB off".into(), |p| format!("USB 127.0.0.1:{p}"));
                let lan = lan_port.map_or("Wi-Fi off".into(), |p| format!("Wi-Fi port {p} (TLS)"));
                self.listen_row.set_subtitle(&format!("{usb} · {lan}"));
                self.sync_accept();
                if !self.streaming.get() {
                    self.set_state(
                        if usb_port.is_none() && lan_port.is_none() {
                            "No connection method enabled (turn on USB or Wi-Fi in Settings)"
                        } else {
                            "Waiting for a tablet"
                        },
                        false,
                    );
                }
            }
            Event::Stopped => {
                self.listen_row.set_subtitle("Not listening");
                self.sync_accept();
                self.clear_session_rows();
                self.set_state("Not accepting tablets", false);
            }
            Event::Connected { device, transport } => {
                self.tablet_row.set_subtitle(&format!("{device} over {}", transport_name(transport)));
                self.set_state("Connecting…", false);
                if prefs.notify_connection {
                    notify::send("Tablet connected".into(), format!("{device} over {}", transport_name(transport)));
                }
            }
            Event::Streaming { width, height, fps, zero_copy } => {
                self.panel.set((width, height));
                self.update_scale_subtitle(self.config.borrow().scale);
                self.show_video(width, height, fps, zero_copy);
                self.set_state("Streaming", true);
            }
            Event::FpsChanged { fps } => {
                if let Some((w, h, _, zc)) = self.video.get() {
                    self.show_video(w, h, fps, zc);
                }
            }
            Event::HostStats { fps, mbps, target_mbps, encode_ms, skipped, queue_ms, net_rtt_ms } => {
                let mut s = format!(
                    "{fps:.0} fps · {mbps:.1} of {target_mbps} Mbit/s · encode {encode_ms:.1} ms · \
                     queue {queue_ms:.0} ms · network rtt {net_rtt_ms:.0} ms"
                );
                if skipped > 0 {
                    s.push_str(&format!(" · {skipped} skipped"));
                }
                self.host_row.set_subtitle(&s);
            }
            Event::ClientStats { fps, decode_ms, rtt_ms } => {
                self.client_row.set_subtitle(&format!(
                    "{fps:.0} fps · decode {decode_ms:.1} ms · round trip {rtt_ms:.1} ms"
                ));
            }
            Event::BitrateChanged { target_mbps, lowered } => {
                if lowered && prefs.notify_bitrate {
                    notify::send(
                        "Bitrate lowered".into(),
                        format!("The link can't keep up; now {target_mbps} Mbit/s"),
                    );
                }
            }
            Event::Disconnected { device, error } => {
                self.clear_session_rows();
                self.set_state("Waiting for a tablet", false);
                match error {
                    Some(err) if prefs.notify_errors => {
                        notify::send(format!("{device} disconnected with an error"), err)
                    }
                    _ if prefs.notify_connection => {
                        notify::send("Tablet disconnected".into(), device)
                    }
                    _ => {}
                }
            }
            Event::Rejected { reason } => {
                if reason.contains("unpaired") && prefs.notify_errors {
                    notify::send(
                        "Wi-Fi connection refused".into(),
                        "A tablet that isn't paired tried to connect. Connect it over USB once.".into(),
                    );
                }
            }
            Event::Paired { device } => {
                self.refresh_tablets();
                if prefs.notify_connection {
                    notify::send("Tablet paired".into(), format!("{device} can now connect over Wi-Fi"));
                }
            }
            Event::Error(err) => {
                if prefs.notify_errors {
                    notify::send("UltraDisplay error".into(), err);
                }
            }
        }
    }

    fn show_video(&self, width: u32, height: u32, fps: u32, zero_copy: bool) {
        self.video.set(Some((width, height, fps, zero_copy)));
        self.video_row.set_subtitle(&format!(
            "{width}×{height} @ {fps} Hz · {}",
            if zero_copy { "zero-copy GPU capture" } else { "shared-memory capture" }
        ));
    }

    fn clear_session_rows(&self) {
        self.video.set(None);
        for r in [&self.tablet_row, &self.video_row, &self.host_row, &self.client_row] {
            r.set_subtitle("—");
        }
    }

    fn set_state(&self, state: &str, streaming: bool) {
        self.state_row.set_subtitle(state);
        *self.status.borrow_mut() = state.to_owned();
        self.streaming.set(streaming);
    }
}
