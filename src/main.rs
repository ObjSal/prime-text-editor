mod theme;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use slint_keyos_platform::app_ui;
use slint_keyos_platform::fs::{self, Location, OpenFlags};
use slint_keyos_platform::slint::{ComponentHandle, ModelRc, VecModel};

app_ui!("prime-text-editor");

/// Mutable app state shared across the UI callbacks.
struct State {
    location: Location,
    path: String,             // current directory, always starts with '/'
    open_path: Option<String>, // full path of the file currently in the editor
    show_hidden: bool,        // include dot-prefixed entries in the listing
}

fn app_main(cx: AppContext, ui: AppWindow) {
    log_server::init_wait(env!("CARGO_CRATE_NAME")).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    theme::init(&ui);

    let fs = cx.fs.clone();
    let ui_weak = ui.as_weak();
    let state = Rc::new(RefCell::new(State {
        location: Location::User,
        path: "/".to_string(),
        open_path: None,
        show_hidden: false,
    }));

    // Re-list the current directory and push it into the Browser global.
    let refresh: Rc<dyn Fn()> = {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        Rc::new(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (loc, path, show_hidden) = {
                let s = state.borrow();
                (s.location, s.path.clone(), s.show_hidden)
            };
            let browser = ui.global::<Browser>();

            let mut items: Vec<(bool, String, String)> = Vec::new();
            let mut status = String::new();
            match fs.open_dir(path.as_str(), loc) {
                Ok(dir) => loop {
                    match dir.next_entry() {
                        Ok(Some(entry)) => {
                            if entry.name == "." || entry.name == ".." {
                                continue;
                            }
                            if !show_hidden && entry.name.starts_with('.') {
                                continue;
                            }
                            let info = if entry.is_dir {
                                "Folder".to_string()
                            } else {
                                human_size(entry.len)
                            };
                            items.push((entry.is_dir, entry.name, info));
                        }
                        Ok(None) => break,
                        Err(e) => {
                            status = err_msg(&e);
                            break;
                        }
                    }
                },
                Err(e) => status = err_msg(&e),
            }

            // Folders first, then alphabetical (case-insensitive).
            items.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            });

            let rows: Vec<FileRow> = items
                .into_iter()
                .map(|(is_dir, name, info)| FileRow {
                    name: name.into(),
                    info: info.into(),
                    is_folder: is_dir,
                })
                .collect();

            browser.set_entries(ModelRc::new(VecModel::from(rows)));
            browser.set_path(path.clone().into());
            browser.set_at_root((path == "/").into());
            browser.set_status(status.into());
        })
    };

    // Populate the initial (Internal) listing.
    refresh();

    let callbacks = ui.global::<Callbacks>();

    // Switch storage tab: Internal / Airlock / USB. Resets to that root.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_location_changed(move |idx| {
            {
                let mut s = state.borrow_mut();
                s.location = location_for(idx);
                s.path = "/".to_string();
            }
            refresh();
        });
    }

    // Toggle showing dot-prefixed (hidden) files/folders.
    {
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        callbacks.on_toggle_hidden(move || {
            let now = {
                let mut s = state.borrow_mut();
                s.show_hidden = !s.show_hidden;
                s.show_hidden
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Ui>().set_show_hidden(now);
            }
            refresh();
        });
    }

    // Tap a row: descend into a folder, or open a file in the editor.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        callbacks.on_entry_activated(move |name, is_folder| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (loc, dir) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let full = join_path(&dir, name.as_str());

            if is_folder {
                state.borrow_mut().path = full;
                refresh();
                return;
            }

            match read_text(&fs, &full, loc) {
                Ok(text) => {
                    state.borrow_mut().open_path = Some(full);
                    let editor = ui.global::<Editor>();
                    editor.set_content(text.into());
                    editor.set_filename(name);
                    show_info(&ui, "");
                    ui.global::<Ui>().set_editing(true);
                }
                Err(msg) => show_error(&ui, msg),
            }
        });
    }

    // Back button: go up one directory.
    {
        let state = state.clone();
        let refresh = refresh.clone();
        callbacks.on_go_back(move || {
            {
                let mut s = state.borrow_mut();
                s.path = parent_path(&s.path);
            }
            refresh();
        });
    }

    // Create a new (empty) file in the current directory and open it.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_new_file(move |name| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let name = name.to_string();
            if name.trim().is_empty() {
                return;
            }
            let (loc, dir) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let full = join_path(&dir, &name);
            match fs.open_file(full.as_str(), loc, OpenFlags::CREATE) {
                Ok(file) => {
                    drop(file); // create + close, leaving an empty file
                    state.borrow_mut().open_path = Some(full);
                    let editor = ui.global::<Editor>();
                    editor.set_content("".into());
                    editor.set_filename(name.into());
                    show_info(&ui, "");
                    ui.global::<Ui>().set_editing(true);
                }
                Err(e) => show_error(&ui, err_msg(&e)),
            }
        });
    }

    // Create a new folder in the current directory.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        callbacks.on_new_folder(move |name| {
            let name = name.to_string();
            if name.trim().is_empty() {
                return;
            }
            let (loc, dir) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let full = join_path(&dir, &name);
            if let Err(e) = fs.create_dir(full.as_str(), loc) {
                if let Some(ui) = ui_weak.upgrade() {
                    show_error(&ui, err_msg(&e));
                }
            }
            refresh();
        });
    }

    // Save the editor contents back to the open file.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        callbacks.on_save_file(move || {
            log::info!("cb: save-file");
            let Some(ui) = ui_weak.upgrade() else { return };
            let (loc, full) = {
                let s = state.borrow();
                (s.location, s.open_path.clone())
            };
            let Some(full) = full else { return };
            let content = ui.global::<Editor>().get_content().to_string();
            let result = fs
                .open_file(full.as_str(), loc, OpenFlags::CREATE)
                .and_then(|mut f| f.overwrite(content.as_bytes()));
            match result {
                Ok(()) => show_info(&ui, "Saved"),
                Err(e) => show_error(&ui, err_msg(&e)),
            }
        });
    }

    // Leave the editor and return to the (refreshed) browser.
    {
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        callbacks.on_close_editor(move || {
            log::info!("cb: close-editor");
            state.borrow_mut().open_path = None;
            if let Some(ui) = ui_weak.upgrade() {
                ui.global::<Ui>().set_editing(false);
            }
            refresh();
        });
    }

    // Delete an entry in the current directory.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        callbacks.on_delete_entry(move |name| {
            let (loc, dir) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let full = join_path(&dir, name.as_str());
            if let Err(e) = fs.remove(full.as_str(), loc) {
                if let Some(ui) = ui_weak.upgrade() {
                    show_error(&ui, err_msg(&e));
                }
            }
            refresh();
        });
    }

    // Rename an entry within the current directory.
    {
        let fs = fs.clone();
        let state = state.clone();
        let ui_weak = ui_weak.clone();
        let refresh = refresh.clone();
        callbacks.on_rename_entry(move |from, to| {
            let to = to.to_string();
            if to.trim().is_empty() {
                return;
            }
            let (loc, dir) = {
                let s = state.borrow();
                (s.location, s.path.clone())
            };
            let from_full = join_path(&dir, from.as_str());
            let to_full = join_path(&dir, &to);
            if let Err(e) = fs.rename(from_full.as_str(), to_full.as_str(), loc) {
                if let Some(ui) = ui_weak.upgrade() {
                    show_error(&ui, err_msg(&e));
                }
            }
            refresh();
        });
    }

    ui.run().expect("UI running");
}

/// Read a file as UTF-8 text; returns a user-facing message on failure.
fn read_text(
    fs: &fs::FileSystem<fs_permissions::FileSystemPermissions>,
    path: &str,
    loc: Location,
) -> Result<String, String> {
    let mut file = fs
        .open_file(path, loc, OpenFlags::READ_ONLY)
        .map_err(|e| err_msg(&e))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|_| "Read failed".to_string())?;
    String::from_utf8(buf).map_err(|_| "Not a text file".to_string())
}

fn show_info(ui: &AppWindow, msg: &str) {
    let u = ui.global::<Ui>();
    u.set_message(msg.into());
    u.set_message_error(false);
}

fn show_error(ui: &AppWindow, msg: String) {
    let u = ui.global::<Ui>();
    u.set_message(msg.into());
    u.set_message_error(true);
}

fn location_for(index: i32) -> Location {
    match index {
        1 => Location::Airlock,
        2 => Location::Usb,
        _ => Location::User,
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn err_msg(e: &fs::Error) -> String {
    use slint_keyos_platform::fs::Error::*;
    match e {
        NoMedia => "Not connected".to_string(),
        AccessDenied => "Access denied".to_string(),
        FileNotFound => "Not found".to_string(),
        FileAlreadyExists => "Already exists".to_string(),
        FileInUse => "File is in use".to_string(),
        InvalidPath => "Invalid name".to_string(),
        other => format!("Error: {other:?}"),
    }
}
