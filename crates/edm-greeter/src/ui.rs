//! Blitz host: turns an interface's HTML into a window and wires it to the [`Controller`].
//!
//! UNVERIFIED AGAINST A COMPILER: written from the docs.rs pages of blitz-dom 0.2.x,
//! blitz-html 0.2.0 and blitz-shell 0.2.3 without being able to fetch the crates. Every line that
//! depends on a Blitz API detail is tagged `VERIFY` so the first `cargo check` is quick to fix.
//!
//! How it works (no JavaScript): the page is plain HTML. Elements are found once by id; the
//! document's UI events are intercepted ([`EventHandler`]), clicks on known ids / Enter in a field
//! call the controller, and the result is written back by editing the DOM (`set_inner_html`,
//! `class` attribute).

use std::any::Any;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blitz_dom::{local_name, ns, BaseDocument, Document, DocumentConfig, DocumentMutator, EventDriver, EventHandler, QualName};
use blitz_html::HtmlDocument;
use blitz_shell::{create_default_event_loop, BlitzApplication, BlitzShellEvent, WindowConfig};
use blitz_traits::events::{BlitzKeyEvent, DomEvent, DomEventData, EventState, UiEvent}; // VERIFY: EventState path
use keyboard_types::{Key, Modifiers}; // same crate/version blitz-traits uses (0.7)
use zeroize::Zeroizing;

use crate::controller::{Backend, Controller, Outcome, Reveal};
use crate::interface::{self, Picture};

/// Ignore a second login trigger within this window (Enter may arrive as KeyDown + KeyPress,
/// and a double-click must not run PAM / consume an HOTP code twice).
const LOGIN_DEBOUNCE: Duration = Duration::from_millis(500);

/// A key that was pressed and never reported as released is forgotten after this long (a lost
/// KeyUp must not block that key forever). Real auto-repeat is flagged by the keyboard and is
/// never filtered.
const KEY_STALE: Duration = Duration::from_millis(1000);

pub struct Ids {
    username: usize,
    password: usize,
    login: usize,
    password_toggle: Option<usize>,
    /// Optional `#password-mask` span: Blitz 0.2.4 has no password masking, so the host keeps the
    /// real text itself and draws dots (or the text, when revealed) here. The input stays empty.
    password_mask: Option<usize>,
    /// Optional label spans inside the toggle: the host swaps their `hidden` class instead of
    /// rewriting the button's children (that made the button disappear in Blitz).
    toggle_show: Option<usize>,
    toggle_hide: Option<usize>,
    twofactor: Option<usize>,
    twofactor_row: Option<usize>,
    error: Option<usize>,
    sessions: Option<usize>,
    themes: Option<usize>,
    restart: Option<usize>,
    /// Picture inspector (all optional): background layer, eye button, panel, info line, file list.
    bg: Option<usize>,
    bg_pic: Option<usize>,
    username_hint: Option<usize>,
    password_hint: Option<usize>,
    twofactor_hint: Option<usize>,
    inspector_toggle: Option<usize>,
    inspector: Option<usize>,
    inspector_info: Option<usize>,
    wallpapers: Option<usize>,
    /// Optional search box of the inspector (`#wallpaper-search`) and its label span.
    search: Option<usize>,
    search_hint: Option<usize>,
}

impl Ids {
    pub fn resolve(doc: &BaseDocument) -> Result<Self, String> {
        let find = |id: &str| doc.get_element_by_id(id); // VERIFY: Option<usize>
        let (u, p, l) = (find("username"), find("password"), find("login"));
        let missing: Vec<&str> = [("username", u), ("password", p), ("login", l)]
            .into_iter()
            .filter(|(_, n)| n.is_none())
            .map(|(name, _)| name)
            .collect();
        match (u, p, l) {
            (Some(username), Some(password), Some(login)) => Ok(Self {
                username,
                password,
                login,
                password_toggle: find("password-toggle"),
                password_mask: find("password-mask"),
                toggle_show: find("password-toggle-show"),
                toggle_hide: find("password-toggle-hide"),
                twofactor: find("twofactor"),
                twofactor_row: find("twofactor-row"),
                error: find("error"),
                sessions: find("sessions"),
                themes: find("themes"),
                restart: find("restart"),
                bg: find("bg"),
                bg_pic: find("bg-pic"),
                username_hint: find("username-hint"),
                password_hint: find("password-hint"),
                twofactor_hint: find("twofactor-hint"),
                inspector_toggle: find("inspector-toggle"),
                inspector: find("inspector"),
                inspector_info: find("inspector-info"),
                wallpapers: find("wallpapers"),
                search: find("wallpaper-search"),
                search_hint: find("wallpaper-search-hint"),
            }),
            _ => Err(format!("interface is missing required element id(s): {}", missing.join(", "))),
        }
    }
}

/// Parse `html` once and check it has the required ids (used to decide on the built-in fallback).
pub fn check_interface(html: &str) -> Result<(), String> {
    let doc = HtmlDocument::from_html(html, DocumentConfig::default()); // VERIFY: sig per docs.rs 0.2.0
    Ids::resolve(&doc).map(|_| ())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn set_class(m: &mut DocumentMutator<'_>, node: usize, class: &str) {
    m.set_attribute(node, QualName::new(None, ns!(), local_name!("class")), class); // VERIFY
}

fn is_enter(k: &BlitzKeyEvent) -> bool {
    // Debug-format compare avoids depending on the keyboard-types version Blitz pins.
    format!("{:?}", k.key) == "Enter" // VERIFY: field name `key`
}

fn is_backspace(k: &BlitzKeyEvent) -> bool {
    format!("{:?}", k.key) == "Backspace" // VERIFY: same Debug form as is_enter
}

struct Logic<B: Backend> {
    ctl: Controller<B>,
    ids: Ids,
    /// Text of the inputs, tracked from Input events (password is wiped when replaced/dropped).
    values: HashMap<usize, Zeroizing<String>>,
    /// Session/theme pickers show ONE entry at a time; wheel or click swaps it for the next one.
    session_cur: Option<usize>,
    theme_cur: Option<usize>,
    theme_idx: usize,
    last_login: Option<Instant>,
    /// Physical keys currently held (Debug of `code` -> time of the KeyDown). A second KeyDown of
    /// a key that was not released in between is the same press delivered twice.
    pressed: HashMap<String, Instant>,
    reveal: Reveal,
    pictures: Vec<Picture>,
    /// `picture_nodes[k]` is the button of `pictures[visible[k]]`: the list shows only what matches `query`.
    visible: Vec<usize>,
    picture_nodes: Vec<Option<usize>>,
    /// Current text of the inspector's search box.
    query: String,
    /// Index summary line (folders, counts); the match count is appended while searching.
    summary: String,
    inspector_open: bool,
    picked: Option<usize>,
    /// `user_dirs` from enginedm.conf (empty = the default Pictures folder).
    user_dirs: Vec<edm_core::config::UserDir>,
    /// enginedm.conf; the chosen picture is written to its `wallpaper=` line.
    config_path: std::path::PathBuf,
    /// Resolved `wallpaper=` of the config, applied once by `init`.
    startup_wallpaper: Option<std::path::PathBuf>,
    /// `user_settings { filter=, filter_known= }`: what is listed while the search box is empty.
    settings: edm_core::config::UserSettings,
    /// Those entries as indices into `pictures` (in config order).
    known: Vec<usize>,
}

impl<B: Backend> Logic<B> {
    /// Redraw the password in `#password-mask`: dots while hidden, the (escaped) text while shown.
    /// Show a field's label span while the field is empty (Blitz draws no `placeholder`).
    fn render_hints(&self, m: &mut DocumentMutator<'_>) {
        let fields = [
            (self.ids.username_hint, Some(self.ids.username)),
            (self.ids.password_hint, Some(self.ids.password)),
            (self.ids.twofactor_hint, self.ids.twofactor),
            (self.ids.search_hint, self.ids.search),
        ];
        for (hint, field) in fields {
            if let (Some(h), Some(f)) = (hint, field) {
                let filled = self.values.get(&f).is_some_and(|v| !v.is_empty());
                set_class(m, h, if filled { "hint hidden" } else { "hint" });
            }
        }
    }

    fn render_password(&self, m: &mut DocumentMutator<'_>) {
        self.render_hints(m);
        let Some(mask) = self.ids.password_mask else { return };
        let real = self.value(self.ids.password);
        let html = if self.reveal.is_visible() {
            escape(&real).replace(' ', "&nbsp;")
        } else {
            "\u{2022}".repeat(real.chars().count())
        };
        m.set_inner_html(mask, &html);
    }

    fn debug_input() -> bool {
        std::env::var_os("EDM_DEBUG_INPUT").is_some()
    }

    /// True for the first KeyDown of a physical key; false for the same press arriving again
    /// (no KeyUp in between, not flagged as auto-repeat).
    fn accept_key_down(&mut self, k: &BlitzKeyEvent) -> bool {
        let id = format!("{:?}", k.code); // VERIFY: field name `code`
        let now = Instant::now();
        if !k.is_auto_repeating {
            // VERIFY: field name `is_auto_repeating`
            if let Some(t) = self.pressed.get(&id) {
                if t.elapsed() < KEY_STALE {
                    if Self::debug_input() {
                        eprintln!("edm: duplicate KeyDown {id} ignored (no KeyUp in between)");
                    }
                    return false;
                }
            }
        }
        self.pressed.insert(id, now);
        true
    }

    fn on_key_up(&mut self, k: &BlitzKeyEvent) {
        self.pressed.remove(&format!("{:?}", k.code)); // VERIFY: field name `code`
    }

    /// The typed character comes from the key event, not from the text editor's Input event.
    /// Every key press is one KeyDown; Input events were seen twice per key.
    fn on_password_char(&mut self, ch: &str, m: &mut DocumentMutator<'_>) {
        if ch.is_empty() {
            return;
        }
        self.values
            .entry(self.ids.password)
            .or_insert_with(|| Zeroizing::new(String::new()))
            .push_str(ch);
        if Self::debug_input() {
            let total = self.values.get(&self.ids.password).map_or(0, |v| v.chars().count());
            eprintln!("edm: password key: +{} char(s), password now {total} char(s)", ch.chars().count());
        }
        self.render_password(m);
    }

    /// With a mask element the input only ever holds the chunk just typed/pasted: append it to the
    /// real password, then empty the input again so the text never sits in it.
    fn on_password_input(&mut self, chunk: &str, m: &mut DocumentMutator<'_>) {
        // Whatever the editor received, empty it again: the text never stays in the input.
        m.set_attribute(self.ids.password, QualName::new(None, ns!(), local_name!("value")), ""); // VERIFY: resets the editor (mutator.rs:233)
        // Blitz 0.2.4's text editor inserts every key twice (seen in the log: one KeyDown, then an
        // Input event of 2 chars), so its text is never trusted: the password comes only from
        // KeyDown. No paste support for now (a paste cannot be told apart from the doubled text).
        if Self::debug_input() {
            eprintln!("edm: password Input event of {} char(s) ignored", chunk.chars().count());
        }
    }

    /// The input is empty, so Backspace produces no Input event: remove the last char here.
    fn on_password_backspace(&mut self, m: &mut DocumentMutator<'_>) {
        if let Some(v) = self.values.get_mut(&self.ids.password) {
            v.pop();
        }
        self.render_password(m);
    }

    /// Show/hide the password (optional `#password-toggle` button).
    fn toggle_reveal(&mut self, m: &mut DocumentMutator<'_>) {
        self.reveal.toggle();
        if self.ids.password_mask.is_some() {
            self.render_password(m);
        } else {
            // No mask element: fall back to <input type>. Blitz 0.2.4 does not mask passwords at all.
            m.set_attribute(
                self.ids.password,
                QualName::new(None, ns!(), local_name!("type")),
                self.reveal.input_type(),
            );
        }
        // Label: the "Show" span is visible while the password is hidden, "Hide" while it is shown.
        let revealed = self.reveal.is_visible();
        if let Some(n) = self.ids.toggle_show {
            set_class(m, n, if revealed { "hidden" } else { "" });
        }
        if let Some(n) = self.ids.toggle_hide {
            set_class(m, n, if revealed { "" } else { "hidden" });
        }
    }

    fn init(&mut self, m: &mut DocumentMutator<'_>) {
        if let Some(row) = self.ids.twofactor_row {
            let required = self.ctl.backend.two_factor_required();
            set_class(m, row, if required { "field" } else { "field hidden" });
        }
        self.render_sessions(m);
        self.render_themes(m);
        self.scan_pictures(m);
        // NOT here: the startup wallpaper is applied on the first UI event (GreeterDoc), after the
        // first style resolve. Adding a node with a background-image before any style exists panics
        // in stylo (data.rs: unwrap on None).
    }

    /// Scan the pictures folder (std only) and fill `#wallpapers` / `#inspector-info`.
    fn scan_pictures(&mut self, m: &mut DocumentMutator<'_>) {
        // Folders come from `user_dirs` in enginedm.conf; without any, `$HOME/Pictures/*`.
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(std::path::PathBuf::from);
        let dirs = if self.user_dirs.is_empty() { interface::default_user_dirs() } else { self.user_dirs.clone() };
        let index = interface::index_pictures(&dirs, home.as_deref());
        let summary = interface::index_summary(&index);
        eprintln!("edm: pictures: {summary}");
        self.pictures = index.pictures;
        // `filter=` entries: found in the index (then that entry is used) or added as extra pictures.
        let bases: Vec<std::path::PathBuf> =
            dirs.iter().filter_map(|d| interface::resolve_user_dir(d, home.as_deref()).ok()).collect();
        let (known, problems) = interface::known_pictures(&self.settings.filter, &bases, home.as_deref());
        self.known.clear();
        for k in known {
            let i = match self.pictures.iter().position(|p| p.path == k.path) {
                Some(i) => i,
                None => {
                    self.pictures.push(k);
                    self.pictures.len() - 1
                }
            };
            self.known.push(i);
        }
        let mut summary = summary;
        for p in &problems {
            eprintln!("edm: {p}");
            summary = format!("{summary} | {p}");
        }
        self.summary = summary;
        self.render_pictures(m);
    }

    /// (Re)draw `#wallpapers` for the current search text and update the info line.
    fn render_pictures(&mut self, m: &mut DocumentMutator<'_>) {
        // While the box is empty, `user_settings` decides what is listed (nothing by default: drawing
        // hundreds of unasked buttons slows the login screen down):
        //   filter=... (some found)  -> those, limited to the first `filter_known` of them if set
        //   no usable filter=        -> the first `filter_known` pictures alphabetically (index is sorted)
        //   neither                  -> nothing until something is typed
        let searching = !self.query.trim().is_empty();
        let from_filter = !self.known.is_empty();
        let (visible, total) = if searching {
            interface::filter_pictures(&self.pictures, &self.query, interface::PICTURE_SHOWN_MAX)
        } else if from_filter {
            let n = self.settings.filter_known.unwrap_or(usize::MAX);
            (self.known.iter().copied().take(n).collect(), self.known.len())
        } else {
            let n = self.settings.filter_known.unwrap_or(0).min(edm_core::config::USER_FILTER_MAX);
            (interface::filter_pictures(&self.pictures, "", n).0, self.pictures.len())
        };
        let labels: Vec<String> = visible
            .iter()
            .map(|&i| format!("{}  ({})", self.pictures[i].name, interface::human_size(self.pictures[i].size)))
            .collect();
        self.visible = visible;
        let selected = self.picked.and_then(|p| self.visible.iter().position(|&i| i == p));
        self.picture_nodes = Self::render_list(m, self.ids.wallpapers, "pic", &labels, selected);
        self.mark_selected(m);
        let mut info = self.summary.clone();
        if !self.query.trim().is_empty() {
            info = if total == 0 {
                format!("no match for '{}' | {info}", self.query.trim())
            } else if total > self.visible.len() {
                format!("{total} match(es), showing {} | {info}", self.visible.len())
            } else {
                format!("{total} match(es) | {info}")
            };
        } else if self.visible.is_empty() {
            info = format!("{total} picture(s) indexed, type to search | {info}");
        } else if from_filter {
            info = format!("{} of {total} picture(s) from filter=, type to search for more | {info}", self.visible.len());
        } else {
            info = format!("first {} of {total} (A-Z), type to search | {info}", self.visible.len());
        }
        if let Some(n) = self.ids.inspector_info {
            m.set_inner_html(n, &escape(&info));
        }
    }

    /// render_list gives buttons class "opt"; the inspector wants "pic" (+ "selected" for the chosen one).
    fn mark_selected(&self, m: &mut DocumentMutator<'_>) {
        for (k, n) in self.picture_nodes.iter().enumerate() {
            if let Some(n) = n {
                let chosen = self.visible.get(k).is_some_and(|&i| Some(i) == self.picked);
                set_class(m, *n, if chosen { "pic selected" } else { "pic" });
            }
        }
    }

    /// The search box changed: filter the list.
    fn set_search(&mut self, text: &str, m: &mut DocumentMutator<'_>) {
        if self.query == text {
            return;
        }
        self.query = text.to_string();
        self.render_pictures(m);
    }

    fn toggle_inspector(&mut self, m: &mut DocumentMutator<'_>) {
        self.inspector_open = !self.inspector_open;
        if let Some(n) = self.ids.inspector {
            set_class(m, n, if self.inspector_open { "" } else { "hidden" });
        }
    }

    /// Put a `data:` URI behind the page. Used both for a picture chosen in the inspector and for the
    /// configured `wallpaper=` at startup, so both go through the one path known to render.
    fn apply_background(&self, uri: &str, m: &mut DocumentMutator<'_>) {
        let Some(bg) = self.ids.bg else { return };
        let style = interface::bg_style(uri);
        if let Some(holder) = self.ids.bg_pic {
            // A brand-new element gets its style (and image) resolved reliably.
            m.set_inner_html(holder, &format!(r#"<div class="pic-layer" style="{}"></div>"#, escape(&style))); // VERIFY
        } else {
            m.set_attribute(bg, QualName::new(None, ns!(), local_name!("style")), &style);
        }
        set_class(m, bg, "has-pic");
    }

    /// The `wallpaper=` from enginedm.conf, shown at startup (before the first frame).
    fn apply_startup_wallpaper(&mut self, m: &mut DocumentMutator<'_>) {
        let Some(path) = self.startup_wallpaper.take() else { return };
        if self.ids.bg.is_none() {
            eprintln!("edm: wallpaper ignored: the interface has no #bg element");
            return;
        }
        match interface::wallpaper_data_uri(&path) {
            Ok(uri) => {
                self.apply_background(&uri, m);
                // Mark it in the inspector list when it lives in one of the indexed folders.
                self.picked = self.pictures.iter().position(|p| p.path == path);
                self.mark_selected(m);
                eprintln!("edm: wallpaper {} loaded at startup ({} bytes embedded)", path.display(), uri.len());
            }
            Err(e) => eprintln!("edm: wallpaper {} ignored: {e}", path.display()),
        }
    }

    /// Read picture `i` as raw bytes and put it behind the page (`style` attribute of `#bg`).
    /// `k` is the position in the (filtered) list.
    fn pick_picture(&mut self, k: usize, m: &mut DocumentMutator<'_>) {
        if self.ids.bg.is_none() {
            return;
        }
        let Some(&i) = self.visible.get(k) else { return };
        let Some(pic) = self.pictures.get(i) else { return };
        match interface::picture_data_uri(&pic.path) {
            Ok(uri) => {
                self.apply_background(&uri, m);
                self.picked = Some(i);
                // Remember the choice: point `wallpaper=` in enginedm.conf at the picture (no copy), so
                // the next start shows it too. Needs write access to the config (root); an
                // unprivileged greeter just keeps the background for this session.
                let saved = match pic.path.to_str().ok_or_else(|| "path is not valid UTF-8".to_string()).and_then(|p| {
                    edm_core::config::set_config_value(&self.config_path, "wallpaper", p)
                }) {
                    Ok(()) => {
                        eprintln!("edm: wallpaper={} saved to {}", pic.path.display(), self.config_path.display());
                        String::new()
                    }
                    Err(e) => {
                        eprintln!("edm: wallpaper not saved to {}: {e}", self.config_path.display());
                        format!(" (not saved to the config: {e})")
                    }
                };
                self.show(m, "info", &format!("Background: {}{saved}", pic.name));
                eprintln!("edm: background set to {} ({} bytes embedded)", pic.name, uri.len());
                self.mark_selected(m);
            }
            Err(e) => self.show(m, "error", &format!("{}: {e}", pic.name)),
        }
    }

    fn render_list(
        m: &mut DocumentMutator<'_>,
        container: Option<usize>,
        prefix: &str,
        labels: &[String],
        selected: Option<usize>,
    ) -> Vec<Option<usize>> {
        let Some(container) = container else { return Vec::new() };
        let mut html = String::new();
        for (i, label) in labels.iter().enumerate() {
            let class = if Some(i) == selected { "opt selected" } else { "opt" };
            html += &format!(r#"<button type="button" id="{prefix}-{i}" class="{class}">{}</button>"#, escape(label));
        }
        m.set_inner_html(container, &html); // VERIFY: needs the html parser provider (set by HtmlDocument)
        m.flush(); // VERIFY: so get_element_by_id can see the new nodes
        (0..labels.len()).map(|i| m.doc.get_element_by_id(&format!("{prefix}-{i}"))).collect() // VERIFY: `doc` field
    }

    /// One button holding the current entry. It is a NEW element on every change, so a CSS
    /// animation on `.swap` (see style.css) plays each time: the old entry is replaced by the next.
    fn render_carousel(m: &mut DocumentMutator<'_>, container: Option<usize>, prefix: &str, label: &str, pos: usize, total: usize) -> Option<usize> {
        let container = container?;
        let html = format!(
            r#"<button type="button" id="{prefix}-cur" class="carousel swap"><span class="cl">{}</span><span class="cn">{}/{}</span></button>"#,
            escape(label),
            pos + 1,
            total
        );
        m.set_inner_html(container, &html); // VERIFY: as render_list
        m.flush();
        m.doc.get_element_by_id(&format!("{prefix}-cur")) // VERIFY: `doc` field
    }

    fn render_sessions(&mut self, m: &mut DocumentMutator<'_>) {
        let names: Vec<String> = self.ctl.backend.sessions().iter().map(|s| s.name.clone()).collect();
        let i = self.ctl.selected_session();
        let label = names.get(i).cloned().unwrap_or_else(|| "No sessions".into());
        self.session_cur = Self::render_carousel(m, self.ids.sessions, "session", &label, i, names.len().max(1));
    }

    fn render_themes(&mut self, m: &mut DocumentMutator<'_>) {
        let names = self.ctl.backend.themes();
        let label = names.get(self.theme_idx).cloned().unwrap_or_else(|| "No themes".into());
        self.theme_cur = Self::render_carousel(m, self.ids.themes, "theme", &label, self.theme_idx, names.len().max(1));
    }

    /// `dir` = +1 next, -1 previous (wraps around).
    fn cycle_session(&mut self, dir: i32, m: &mut DocumentMutator<'_>) {
        let n = self.ctl.backend.sessions().len() as i32;
        if n == 0 {
            return;
        }
        let i = (self.ctl.selected_session() as i32 + dir).rem_euclid(n) as usize;
        self.ctl.select_session(i);
        self.render_sessions(m);
    }

    fn cycle_theme(&mut self, dir: i32, m: &mut DocumentMutator<'_>) {
        let n = self.ctl.backend.themes().len() as i32;
        if n == 0 {
            return;
        }
        let i = (self.theme_idx as i32 + dir).rem_euclid(n) as usize;
        match self.ctl.apply_theme(i) {
            Ok(name) => {
                self.theme_idx = i;
                self.show(m, "info", &format!("Theme: {name}"));
            }
            Err(e) => self.show(m, "error", &e),
        }
        self.render_themes(m);
    }

    fn show(&self, m: &mut DocumentMutator<'_>, class: &str, text: &str) {
        if let Some(n) = self.ids.error {
            set_class(m, n, &format!("msg {class} visible"));
            m.set_inner_html(n, &escape(text));
        }
    }

    fn value(&self, node: usize) -> Zeroizing<String> {
        Zeroizing::new(self.values.get(&node).map(|v| v.to_string()).unwrap_or_default())
    }

    fn is_form_field(&self, node: usize) -> bool {
        node == self.ids.username || node == self.ids.password || Some(node) == self.ids.twofactor
    }

    fn do_login(&mut self, m: &mut DocumentMutator<'_>) {
        if self.last_login.is_some_and(|t| t.elapsed() < LOGIN_DEBOUNCE) {
            return;
        }
        self.last_login = Some(Instant::now());
        let user = self.value(self.ids.username);
        let pass = self.value(self.ids.password);
        let code = self.ids.twofactor.map(|n| self.value(n)).unwrap_or_default();
        match self.ctl.login(&user, &pass, &code) {
            Outcome::Started(name) => {
                self.values.remove(&self.ids.password);
                self.render_password(m);
                self.show(m, "info", &format!("Starting {name}..."));
                if self.ctl.backend.should_exit() {
                    // greetd: the session starts after this process is gone. The password cell
                    // was already wiped above; skipping destructors here is intended.
                    std::process::exit(0);
                }
            }
            Outcome::Failed(msg) => self.show(m, "error", &msg),
        }
    }

    fn on_click(&mut self, chain: &[usize], m: &mut DocumentMutator<'_>) {
        let hit = |n: usize| chain.contains(&n);
        if hit(self.ids.login) {
            return self.do_login(m);
        }
        if self.ids.password_toggle.is_some_and(hit) {
            return self.toggle_reveal(m);
        }
        if self.ids.inspector_toggle.is_some_and(hit) {
            return self.toggle_inspector(m);
        }
        if let Some(i) = self.picture_nodes.iter().position(|n| n.is_some_and(hit)) {
            return self.pick_picture(i, m);
        }
        if self.ids.restart.is_some_and(hit) {
            match self.ctl.restart() {
                Ok(()) => self.show(m, "info", "Session restart requested"),
                Err(e) => self.show(m, "error", &e),
            }
            return;
        }
        if self.session_cur.is_some_and(hit) {
            return self.cycle_session(1, m);
        }
        if self.theme_cur.is_some_and(hit) {
            self.cycle_theme(1, m);
        }
    }
}

struct Handler<B: Backend>(Arc<Mutex<Logic<B>>>);

impl<B: Backend> EventHandler for Handler<B> {
    fn handle_event(&mut self, chain: &[usize], event: &mut DomEvent, m: &mut DocumentMutator<'_>, state: &mut EventState) {
        let Ok(mut l) = self.0.lock() else { return };
        if Logic::<B>::debug_input() {
            // Timeline of every key-related event (no key contents, so no password leaks).
            use std::sync::OnceLock;
            static T0: OnceLock<Instant> = OnceLock::new();
            let ms = T0.get_or_init(Instant::now).elapsed().as_micros();
            let field = if event.target == l.ids.password { "password" } else if event.target == l.ids.username { "username" } else { "other" };
            match &event.data {
                DomEventData::KeyDown(k) => eprintln!("edm: [{ms:>9} us] KeyDown  on {field} repeat={} chain_len={}", k.is_auto_repeating, chain.len()),
                DomEventData::KeyUp(_) => eprintln!("edm: [{ms:>9} us] KeyUp    on {field}"),
                DomEventData::Input(e) => eprintln!("edm: [{ms:>9} us] Input    on {field} len={}", e.value.chars().count()),
                _ => {}
            }
        }
        match &event.data {
            // VERIFY: payload field `value` holds the full text of the input
            DomEventData::Input(e) => {
                if l.ids.search.is_some_and(|n| n == event.target) {
                    // Wallpaper search box: filter the inspector list (idempotent, so bubbling is harmless).
                    l.values.insert(event.target, Zeroizing::new(e.value.clone()));
                    l.render_hints(m);
                    l.set_search(&e.value, m);
                } else if l.ids.password_mask.is_some() && event.target == l.ids.password {
                    // Handle the keystroke exactly once. Blitz calls this handler for every
                    // bubbling step of the same event; a timing-based filter alone lets the repeat
                    // through whenever the greeter is slow (software rendering, right after
                    // start), which drew two dots per key. Only the first step (the target
                    // itself) counts, and propagation is stopped so no later step sees it.
                    if chain.first().is_some_and(|n| *n != event.target) {
                        return;
                    }
                    l.on_password_input(&e.value, m);
                    state.stop_propagation(); // VERIFY: EventState::stop_propagation
                } else {
                    l.values.insert(event.target, Zeroizing::new(e.value.clone()));
                    l.render_hints(m);
                }
            }
            DomEventData::KeyUp(k) => l.on_key_up(k), // VERIFY: variant `KeyUp`
            DomEventData::KeyDown(k) if is_backspace(k) && l.ids.password_mask.is_some() && event.target == l.ids.password => {
                if l.accept_key_down(k) {
                    l.on_password_backspace(m)
                }
            }
            // Typed characters in the password field: one KeyDown = one character (Ctrl/Meta
            // combinations are shortcuts, not text).
            DomEventData::KeyDown(k)
                if l.ids.password_mask.is_some()
                    && event.target == l.ids.password
                    && matches!(k.key, Key::Character(_))
                    && !k.modifiers.contains(Modifiers::CONTROL) // VERIFY: field `modifiers`
                    && !k.modifiers.contains(Modifiers::META) =>
            {
                if l.accept_key_down(k) {
                    if let Key::Character(c) = &k.key {
                        l.on_password_char(c, m);
                    }
                }
            }
            // VERIFY: variant names Click / KeyDown and `button` field
            DomEventData::Click(e) if matches!(e.button, blitz_traits::events::MouseEventButton::Main) => l.on_click(chain, m),
            // Up/Down arrows: previous/next session; with Shift: previous/next theme.
            DomEventData::KeyDown(k) if matches!(format!("{:?}", k.key).as_str(), "ArrowDown" | "ArrowUp") => {
                if l.accept_key_down(k) {
                    let dir = if format!("{:?}", k.key) == "ArrowDown" { 1 } else { -1 };
                    if k.modifiers.contains(Modifiers::SHIFT) {
                        l.cycle_theme(dir, m);
                    } else {
                        l.cycle_session(dir, m);
                    }
                }
            }
            DomEventData::KeyDown(k) if is_enter(k) && l.is_form_field(event.target) => l.do_login(m),
            _ => {}
        }
    }
}

pub struct GreeterDoc<B: Backend + 'static> {
    inner: HtmlDocument,
    logic: Arc<Mutex<Logic<B>>>,
}

impl<B: Backend + 'static> Deref for GreeterDoc<B> {
    type Target = BaseDocument;
    fn deref(&self) -> &BaseDocument {
        &self.inner
    }
}

impl<B: Backend + 'static> DerefMut for GreeterDoc<B> {
    fn deref_mut(&mut self) -> &mut BaseDocument {
        &mut self.inner
    }
}

impl<B: Backend + 'static> Document for GreeterDoc<B> {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    // Same shape as the trait's default handle_ui_event, but with our handler instead of the no-op.
    fn handle_ui_event(&mut self, event: UiEvent) {
        // First event = the window is up and styles are resolved: now the wallpaper is safe to add.
        if let Ok(mut l) = self.logic.lock() {
            if l.startup_wallpaper.is_some() {
                let mut m = self.inner.mutate();
                l.apply_startup_wallpaper(&mut m);
            }
        }
        let handler = Handler(self.logic.clone());
        let mutr = self.inner.mutate(); // VERIFY: BaseDocument::mutate() -> DocumentMutator
        let mut driver = EventDriver::new(mutr, handler);
        driver.handle_ui_event(event);
    }
}

/// Wraps the shell's net callback and prints what every resource load produced.
/// Signatures (docs.rs): blitz-traits 0.2.0 `NetCallback::call(&self, usize, Result<Data, Option<String>>)`,
/// blitz-shell 0.2.3 `BlitzShellNetCallback::shared(proxy) -> Arc<dyn NetCallback<Resource>>`.
#[cfg(feature = "wallpaper")]
struct LogCallback(std::sync::Arc<dyn blitz_traits::net::NetCallback<blitz_dom::net::Resource>>);

#[cfg(feature = "wallpaper")]
impl blitz_traits::net::NetCallback<blitz_dom::net::Resource> for LogCallback {
    fn call(&self, doc_id: usize, result: Result<blitz_dom::net::Resource, Option<String>>) {
        match &result {
            Ok(blitz_dom::net::Resource::Image(node, _, w, h, _)) => {
                eprintln!("[edm-net] result: Image decoded node={node} {w}x{h}")
            }
            Ok(_) => eprintln!("[edm-net] result: other resource loaded"),
            Err(e) => eprintln!("[edm-net] result: ERROR {e:?}"),
        }
        self.0.call(doc_id, result);
    }
}

/// Wraps the real provider and prints every request (URL shortened: data: URIs are huge).
#[cfg(feature = "wallpaper")]
struct LogProvider(std::sync::Arc<dyn blitz_traits::net::NetProvider<blitz_dom::net::Resource>>);

#[cfg(feature = "wallpaper")]
impl blitz_traits::net::NetProvider<blitz_dom::net::Resource> for LogProvider {
    fn fetch(&self, doc_id: usize, request: blitz_traits::net::Request, handler: blitz_traits::net::BoxedHandler<blitz_dom::net::Resource>) {
        let url = request.url.as_str();
        let head: String = url.chars().take(40).collect();
        eprintln!("[edm-net] fetch: {head}... (url len {})", url.len());
        self.0.fetch(doc_id, request, handler);
    }
}

pub fn run<B: Backend + 'static>(
    html: &str,
    mut controller: Controller<B>,
    user_dirs: Vec<edm_core::config::UserDir>,
    config_path: std::path::PathBuf,
    startup_wallpaper: Option<std::path::PathBuf>,
    settings: edm_core::config::UserSettings,
) -> Result<(), String> {
    let event_loop = create_default_event_loop::<BlitzShellEvent>(); // VERIFY
    let proxy = event_loop.create_proxy(); // VERIFY

    #[allow(unused_mut)]
    // `set_inner_html` (render_list, error text, ...) needs an HTML parser provider; without one it
    // clears the children and inserts nothing.
    let mut config = DocumentConfig {
        html_parser_provider: Some(std::sync::Arc::new(blitz_html::HtmlProvider)),
        ..Default::default()
    };
    // Wallpaper images (file:// URLs in CSS) need a network provider, which needs a tokio runtime.
    // Optional feature: the default build has no networking and uses the gradient.
    #[cfg(feature = "wallpaper")]
    {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        // The provider grabs `Handle::current()`; the runtime must outlive the event loop.
        let rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(rt));
        let _guard = rt.enter();
        // Diagnostics: LogCallback prints every load RESULT, LogProvider every REQUEST. blitz-net
        // swallows fetch errors (bad data: URI etc.), so "[edm-net] fetch" without a matching
        // "[edm-net] result" line means the fetch itself failed.
        let callback = LogCallback(blitz_shell::BlitzShellNetCallback::shared(proxy.clone()));
        config.net_provider = Some(std::sync::Arc::new(LogProvider(blitz_net::Provider::shared(
            std::sync::Arc::new(callback),
        ))));
        std::mem::forget(_guard);
    }

    let mut inner = HtmlDocument::from_html(html, config);
    let ids = Ids::resolve(&inner)?;
    controller.select_session(controller.selected_session()); // keep preselection
    let mut logic = Logic {
        ctl: controller,
        ids,
        values: HashMap::new(),
        session_cur: None,
        theme_cur: None,
        theme_idx: 0,
        last_login: None,
        pressed: HashMap::new(),
        reveal: Reveal::default(),
        pictures: Vec::new(),
        visible: Vec::new(),
        picture_nodes: Vec::new(),
        query: String::new(),
        summary: String::new(),
        inspector_open: false,
        picked: None,
        user_dirs,
        config_path,
        startup_wallpaper,
        settings,
        known: Vec::new(),
    };
    {
        let mut m = inner.mutate();
        logic.init(&mut m);
    }
    let doc = GreeterDoc { inner, logic: Arc::new(Mutex::new(logic)) };

    let mut app = BlitzApplication::new(proxy); // VERIFY
    let attrs = blitz_shell::Window::default_attributes().with_title("EngineDM").with_maximized(true); // VERIFY: winit 0.30 re-export
    let renderer = anyrender_vello::VelloWindowRenderer::new(); // VERIFY: crate/type name + version
    app.add_window(WindowConfig::with_attributes(Box::new(doc), renderer, attrs)); // signature confirmed on docs.rs
    event_loop.run_app(&mut app).map_err(|e| e.to_string())
}
