//! The Files window: browse a phone's storage, download, upload, rename, delete.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use futures_util::StreamExt;
use pairly_dbus::{DaemonProxy, FileEntry};
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

fn describe(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        other => other.to_string(),
    }
}

fn call<T: Send + 'static>(
    fut: impl Future<Output = zbus::Result<T>> + Send + 'static,
    done: impl FnOnce(Result<T, String>) + 'static,
) {
    gtk::glib::spawn_future_local(async move {
        let result = match relm4::spawn(fut).await {
            Ok(r) => r.map_err(|e| describe(&e)),
            Err(e) => Err(e.to_string()),
        };
        done(result);
    });
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn icon_for(e: &FileEntry) -> &'static str {
    if e.dir {
        return "folder-symbolic";
    }
    let ext = e.name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "bmp" => "image-x-generic-symbolic",
        "mp4" | "mkv" | "webm" | "mov" | "3gp" | "avi" => "video-x-generic-symbolic",
        "mp3" | "m4a" | "ogg" | "opus" | "wav" | "flac" | "aac" => "audio-x-generic-symbolic",
        "zip" | "apk" | "rar" | "7z" | "tar" | "gz" => "package-x-generic-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

fn size(bytes: u64) -> String {
    gtk::glib::format_size(bytes).to_string()
}

fn when(ms: i64) -> String {
    gtk::glib::DateTime::from_unix_local(ms / 1000)
        .ok()
        .and_then(|d| d.format("%e %b %Y, %H:%M").ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

struct Ui {
    daemon: DaemonProxy<'static>,
    device: String,
    window: adw::Window,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    list: gtk::ListBox,
    stack: gtk::Stack,
    status: adw::StatusPage,
    up: gtk::Button,
    progress: gtk::ProgressBar,
    path: RefCell<String>,
    busy: Cell<u32>,
}

pub fn open(
    parent: &impl IsA<gtk::Window>,
    daemon: DaemonProxy<'static>,
    device: String,
    name: &str,
) {
    let up = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Up")
        .build();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh")
        .build();
    let mkdir = gtk::Button::builder()
        .icon_name("folder-new-symbolic")
        .tooltip_text("New Folder")
        .build();
    let upload = gtk::Button::builder()
        .icon_name("document-send-symbolic")
        .tooltip_text("Upload Files Here")
        .build();
    let title = adw::WindowTitle::new(name, "/");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_start(&up);
    header.pack_start(&refresh);
    header.pack_end(&upload);
    header.pack_end(&mkdir);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    let clamp = adw::Clamp::builder()
        .maximum_size(820)
        .child(&list)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let status = adw::StatusPage::builder()
        .icon_name("folder-symbolic")
        .title("Loading…")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&status, Some("status"));
    stack.add_named(&scroller, Some("list"));
    let progress = gtk::ProgressBar::builder().visible(false).build();
    progress.add_css_class("osd");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&progress);
    body.append(&stack);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&body));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));
    let window = adw::Window::builder()
        .title(format!("Files — {name}"))
        .default_width(760)
        .default_height(680)
        .transient_for(parent)
        .content(&view)
        .build();

    let ui = Rc::new(Ui {
        daemon: daemon.clone(),
        device: device.clone(),
        window: window.clone(),
        title,
        toasts,
        list,
        stack,
        status,
        up: up.clone(),
        progress,
        path: RefCell::default(),
        busy: Cell::new(0),
    });

    up.connect_clicked({
        let ui = ui.clone();
        move |_| {
            let parent = {
                let p = ui.path.borrow();
                p.rsplit_once('/')
                    .map_or(String::new(), |(a, _)| a.to_owned())
            };
            ui.go(parent);
        }
    });
    refresh.connect_clicked({
        let ui = ui.clone();
        move |_| ui.reload()
    });
    mkdir.connect_clicked({
        let ui = ui.clone();
        move |_| ui.ask_name("New Folder", "Create", "", move |ui, name| ui.mkdir(name))
    });
    upload.connect_clicked({
        let ui = ui.clone();
        move |_| ui.pick_upload()
    });
    // Drop files from a file manager to upload them here.
    let drop = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    drop.connect_drop({
        let ui = ui.clone();
        move |_, value, _, _| {
            let Ok(list) = value.get::<gtk::gdk::FileList>() else {
                return false;
            };
            let paths: Vec<String> = list
                .files()
                .iter()
                .filter_map(|f| f.path())
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            ui.upload(paths);
            true
        }
    });
    window.add_controller(drop);

    // Progress of downloads and uploads.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, u64)>();
    let watcher = relm4::spawn({
        let device = device.clone();
        async move {
            let Ok(mut stream) = daemon.receive_files_progress().await else {
                return;
            };
            while let Some(s) = stream.next().await {
                if let Ok(a) = s.args()
                    && a.id == device
                    && tx.send((a.done, a.total)).is_err()
                {
                    return;
                }
            }
        }
    });
    gtk::glib::spawn_future_local({
        let ui = ui.clone();
        async move {
            while let Some((done, total)) = rx.recv().await {
                if total > 0 {
                    #[allow(clippy::cast_precision_loss)] // a progress bar
                    ui.progress.set_fraction(done as f64 / total as f64);
                }
            }
        }
    });
    window.connect_close_request(move |_| {
        watcher.abort();
        gtk::glib::Propagation::Proceed
    });

    ui.go(String::new());
    window.present();
}

impl Ui {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn begin(&self) {
        self.busy.set(self.busy.get() + 1);
        self.progress.set_fraction(0.0);
        self.progress.set_visible(true);
    }

    fn end(&self) {
        self.busy.set(self.busy.get().saturating_sub(1));
        if self.busy.get() == 0 {
            self.progress.set_visible(false);
        }
    }

    fn go(self: &Rc<Self>, path: String) {
        *self.path.borrow_mut() = path;
        self.reload();
    }

    fn reload(self: &Rc<Self>) {
        let path = self.path.borrow().clone();
        self.title.set_subtitle(&format!("/{path}"));
        self.up.set_sensitive(!path.is_empty());
        let (daemon, device) = (self.daemon.clone(), self.device.clone());
        let ui = self.clone();
        let p = path.clone();
        call(
            async move { daemon.files_list(&device, &p).await },
            move |r| match r {
                Ok(entries) => ui.show(&path, entries),
                Err(e) => {
                    ui.status.set_icon_name(Some("dialog-warning-symbolic"));
                    ui.status.set_title("Can't Open This Folder");
                    ui.status.set_description(Some(&e));
                    ui.stack.set_visible_child_name("status");
                }
            },
        );
    }

    fn show(self: &Rc<Self>, dir: &str, entries: Vec<FileEntry>) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        if entries.is_empty() {
            self.status.set_icon_name(Some("folder-symbolic"));
            self.status.set_title("Empty Folder");
            self.status
                .set_description(Some("Drop files here to upload them."));
            self.stack.set_visible_child_name("status");
            return;
        }
        for e in entries {
            let subtitle = if e.dir {
                when(e.modified_ms)
            } else {
                format!("{} · {}", size(e.size), when(e.modified_ms))
            };
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&e.name))
                .subtitle(subtitle)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon_for(&e)));
            let path = join(dir, &e.name);
            let button = |icon: &str, tip: &str| {
                let b = gtk::Button::builder()
                    .icon_name(icon)
                    .tooltip_text(tip)
                    .valign(gtk::Align::Center)
                    .build();
                b.add_css_class("flat");
                b
            };
            if !e.dir {
                let download = button("folder-download-symbolic", "Download");
                let (ui, p, s) = (self.clone(), path.clone(), e.size);
                download.connect_clicked(move |_| ui.download(p.clone(), s));
                row.add_suffix(&download);
            }
            let rename = button("document-edit-symbolic", "Rename");
            let (ui, p, name) = (self.clone(), path.clone(), e.name.clone());
            rename.connect_clicked(move |_| {
                let p = p.clone();
                ui.ask_name("Rename", "Rename", &name, move |ui, new| ui.rename(&p, new));
            });
            row.add_suffix(&rename);
            let delete = button("user-trash-symbolic", "Delete");
            let (ui, p, name) = (self.clone(), path.clone(), e.name.clone());
            delete.connect_clicked(move |_| ui.confirm_delete(&p, &name));
            row.add_suffix(&delete);
            let (ui, dir_entry, s) = (self.clone(), e.dir, e.size);
            row.connect_activated(move |_| {
                if dir_entry {
                    ui.go(path.clone());
                } else {
                    ui.download(path.clone(), s);
                }
            });
            self.list.append(&row);
        }
        self.stack.set_visible_child_name("list");
    }

    fn download(self: &Rc<Self>, path: String, size: u64) {
        self.begin();
        let (daemon, device, ui) = (self.daemon.clone(), self.device.clone(), self.clone());
        call(
            async move { daemon.files_download(&device, &path, size).await },
            move |r| {
                ui.end();
                match r {
                    Ok(local) => {
                        let name = std::path::Path::new(&local)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let toast = adw::Toast::new(&format!("Downloaded {name}"));
                        toast.set_button_label(Some("Open"));
                        let window = ui.window.clone();
                        toast.connect_button_clicked(move |_| {
                            gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(&local))).launch(
                                Some(&window),
                                gtk::gio::Cancellable::NONE,
                                |_| {},
                            );
                        });
                        ui.toasts.add_toast(toast);
                    }
                    Err(e) => ui.toast(&format!("Download failed: {e}")),
                }
            },
        );
    }

    fn pick_upload(self: &Rc<Self>) {
        let ui = self.clone();
        gtk::FileDialog::builder()
            .title("Upload to the Phone")
            .accept_label("Upload")
            .build()
            .open_multiple(
                Some(&self.window),
                gtk::gio::Cancellable::NONE,
                move |res| {
                    if let Ok(files) = res {
                        let paths = (0..files.n_items())
                            .filter_map(|i| {
                                files.item(i)?.downcast::<gtk::gio::File>().ok()?.path()
                            })
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect();
                        ui.upload(paths);
                    }
                },
            );
    }

    fn upload(self: &Rc<Self>, paths: Vec<String>) {
        if paths.is_empty() {
            return;
        }
        self.begin();
        let dir = self.path.borrow().clone();
        let (daemon, device, ui) = (self.daemon.clone(), self.device.clone(), self.clone());
        let count = paths.len();
        call(
            async move {
                for p in &paths {
                    daemon.files_upload(&device, p, &dir).await?;
                }
                Ok(())
            },
            move |r| {
                ui.end();
                match r {
                    Ok(()) => ui.toast(&format!("Uploaded {count} file(s)")),
                    Err(e) => ui.toast(&format!("Upload failed: {e}")),
                }
                ui.reload();
            },
        );
    }

    fn mkdir(self: &Rc<Self>, name: String) {
        let path = join(&self.path.borrow(), &name);
        let (daemon, device, ui) = (self.daemon.clone(), self.device.clone(), self.clone());
        call(
            async move { daemon.files_mkdir(&device, &path).await },
            move |r| {
                if let Err(e) = r {
                    ui.toast(&format!("Couldn't create the folder: {e}"));
                }
                ui.reload();
            },
        );
    }

    fn rename(self: &Rc<Self>, from: &str, name: String) {
        let to = join(&self.path.borrow(), &name);
        let from = from.to_owned();
        let (daemon, device, ui) = (self.daemon.clone(), self.device.clone(), self.clone());
        call(
            async move { daemon.files_rename(&device, &from, &to).await },
            move |r| {
                if let Err(e) = r {
                    ui.toast(&format!("Couldn't rename: {e}"));
                }
                ui.reload();
            },
        );
    }

    fn confirm_delete(self: &Rc<Self>, path: &str, name: &str) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete “{name}”?")),
            Some("It's removed from the phone for good."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let (ui, path) = (self.clone(), path.to_owned());
        dialog.connect_response(Some("delete"), move |_, _| {
            let (daemon, device, ui2, p) = (
                ui.daemon.clone(),
                ui.device.clone(),
                ui.clone(),
                path.clone(),
            );
            call(
                async move { daemon.files_delete(&device, &p).await },
                move |r| {
                    if let Err(e) = r {
                        ui2.toast(&format!("Couldn't delete: {e}"));
                    }
                    ui2.reload();
                },
            );
        });
        dialog.present(Some(&self.window));
    }

    /// Ask for a file or folder name, then call `done`.
    fn ask_name(
        self: &Rc<Self>,
        title: &str,
        action: &str,
        initial: &str,
        done: impl Fn(&Rc<Self>, String) + 'static,
    ) {
        let dialog = adw::AlertDialog::new(Some(title), None);
        let entry = adw::EntryRow::builder()
            .title("Name")
            .text(initial)
            .activates_default(true)
            .build();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.append(&entry);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("ok", action)]);
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        let ui = self.clone();
        dialog.connect_response(Some("ok"), move |_, _| {
            let name = entry.text().trim().to_owned();
            if !name.is_empty() && !name.contains('/') {
                done(&ui, name);
            }
        });
        dialog.present(Some(&self.window));
    }
}
