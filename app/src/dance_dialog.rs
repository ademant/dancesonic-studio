//! Editing dances, meter and shoes of one track or a whole list (#16).
//!
//! The dialog collects the changes and queues them all at once; the queue
//! sends them to DanceBrainz in the background. For a list, a dance is added
//! to every track and removed from the tracks that have it; meter and shoes
//! are set on all of them. "Leave as it is" is the default of every field,
//! so a bulk edit changes only what the user chose.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;
use studio_core::dance_edits::{self, Edit, FlushReport, MeterChoice};
use studio_core::dancebrainz::{DANCE_KIND, Mbid};
use studio_core::dancebrainz_device::{Suggestion, TagValue};

use crate::Services;
use crate::contribute::{self, Target};

/// The tracks the dialog may change.
pub(crate) struct Scope {
    /// The track shown in the detail pane.
    pub(crate) one: Mbid,
    /// Every track of the list with a MusicBrainz ID, `one` among them.
    pub(crate) all: Vec<Mbid>,
    /// The list's title.
    pub(crate) list: String,
}

/// Called with the outcome once the changes are queued and a send was tried.
pub(crate) type OnDone = Rc<dyn Fn(Result<Option<FlushReport>, String>)>;

/// What to do with a field that is set, not added to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Setting<T> {
    Keep,
    Clear,
    Set(T),
}

/// The changes the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) add: Vec<(Suggestion, String)>,
    pub(crate) remove: Vec<String>,
    pub(crate) meter: Setting<MeterChoice>,
    pub(crate) shoes: Setting<u8>,
}

impl Plan {
    fn new() -> Self {
        Self {
            add: Vec::new(),
            remove: Vec::new(),
            meter: Setting::Keep,
            shoes: Setting::Keep,
        }
    }
}

/// The queue entries for `plan` on `targets`. Each removal and meter change
/// carries what that track showed, so the send can tell whether DanceBrainz
/// changed in the meantime.
pub(crate) fn edits_for(targets: &[Target], plan: &Plan) -> Vec<(Mbid, Edit)> {
    let mut out = Vec::new();
    for target in targets {
        let shown: Vec<String> = target
            .cached
            .iter()
            .filter(|t| t.kind == DANCE_KIND)
            .map(|t| t.name.clone())
            .collect();
        for name in &plan.remove {
            out.push((
                target.mbid.clone(),
                Edit::RemoveDance {
                    name: name.clone(),
                    shown: shown.clone(),
                },
            ));
        }
        for (dance, qualifier) in &plan.add {
            out.push((
                target.mbid.clone(),
                Edit::AddDance {
                    path: dance.path.clone(),
                    name: dance.name.clone(),
                    qualifier: qualifier.clone(),
                },
            ));
        }
        let meter = match &plan.meter {
            Setting::Keep => None,
            Setting::Clear => Some(None),
            Setting::Set(choice) => Some(Some(choice.clone())),
        };
        if let Some(to) = meter {
            out.push((
                target.mbid.clone(),
                Edit::SetMeter {
                    to,
                    shown: dance_edits::meter_of(&target.cached),
                },
            ));
        }
        match plan.shoes {
            Setting::Keep => {}
            Setting::Clear => out.push((target.mbid.clone(), Edit::SetShoes { to: None })),
            Setting::Set(value) => {
                out.push((target.mbid.clone(), Edit::SetShoes { to: Some(value) }));
            }
        }
    }
    out
}

/// The dances of `targets` with how many tracks have each, by name.
pub(crate) fn dance_counts(targets: &[Target]) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for target in targets {
        let mut seen = Vec::new();
        for tag in target.cached.iter().filter(|t| t.kind == DANCE_KIND) {
            if seen.contains(&tag.name) {
                continue;
            }
            seen.push(tag.name.clone());
            match counts.iter_mut().find(|(name, _)| name == &tag.name) {
                Some((_, count)) => *count += 1,
                None => counts.push((tag.name.clone(), 1)),
            }
        }
    }
    counts
}

const QUALIFIERS: [(&str, &str); 3] = [
    ("both", "to learn and at balls"),
    ("learn", "to learn"),
    ("disco", "at balls"),
];

struct Dialog {
    window: gtk::Window,
    runtime: Arc<tokio::runtime::Runtime>,
    services: Services,
    scope: Scope,
    every: gtk::CheckButton,
    targets: RefCell<Vec<Target>>,
    plan: RefCell<Plan>,
    present_list: gtk::ListBox,
    planned_list: gtk::ListBox,
    search: gtk::SearchEntry,
    suggestions: RefCell<Vec<Suggestion>>,
    suggestion_list: gtk::ListBox,
    qualifier: gtk::DropDown,
    meters: RefCell<Vec<TagValue>>,
    meter_choice: gtk::DropDown,
    meter_names: gtk::StringList,
    sub_choice: gtk::DropDown,
    sub_names: gtk::StringList,
    shoes: gtk::DropDown,
    status: gtk::Label,
    save: gtk::Button,
    on_done: OnDone,
}

/// Shows the dialog for `scope`.
pub(crate) fn present(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    services: Services,
    scope: Scope,
    on_done: OnDone,
) {
    open(parent, runtime, services, scope, on_done);
}

/// Builds and shows the dialog; it keeps itself alive until it is closed.
fn open(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    services: Services,
    scope: Scope,
    on_done: OnDone,
) -> Rc<Dialog> {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Edit dances")
        .default_width(560)
        .default_height(680)
        .build();
    let intro = gtk::Label::builder()
        .label(
            "Changes go to DanceBrainz as this paired device. They wait in a queue \
             while DanceBrainz cannot be reached, and a change to a field that \
             changed on DanceBrainz meanwhile is not sent.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    let only_one = gtk::CheckButton::with_label("This track");
    let every = gtk::CheckButton::with_label(&format!(
        "All {} tracks with a MusicBrainz ID in “{}”",
        scope.all.len(),
        scope.list
    ));
    every.set_group(Some(&only_one));
    only_one.set_active(true);
    let scope_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    scope_row.append(&only_one);
    scope_row.append(&every);
    scope_row.set_visible(scope.all.len() > 1);

    let heading = |text: &str| {
        gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .css_classes(["heading"])
            .margin_top(6)
            .build()
    };
    let list = || {
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        list
    };
    let present_list = list();
    let planned_list = list();
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Find a dance to add")
        .hexpand(true)
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Find a dance to add")]);
    let qualifier = gtk::DropDown::from_strings(&QUALIFIERS.map(|(_, label)| label));
    qualifier.set_tooltip_text(Some("What the track is good for"));
    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    search_row.append(&search);
    search_row.append(&qualifier);
    let suggestion_list = gtk::ListBox::new();
    suggestion_list.set_activate_on_single_click(true);

    let meter_names = gtk::StringList::new(&["Leave as it is", "No meter"]);
    let meter_choice = gtk::DropDown::builder().model(&meter_names).build();
    meter_choice.update_property(&[gtk::accessible::Property::Label("Meter")]);
    let sub_names = gtk::StringList::new(&["No sub-meter"]);
    let sub_choice = gtk::DropDown::builder()
        .model(&sub_names)
        .sensitive(false)
        .build();
    sub_choice.update_property(&[gtk::accessible::Property::Label("Sub-meter")]);
    let meter_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    meter_row.append(&meter_choice);
    meter_row.append(&sub_choice);
    let shoes = gtk::DropDown::from_strings(&[
        "Leave as it is",
        "Clear my rating",
        "1 of 5",
        "2 of 5",
        "3 of 5",
        "4 of 5",
        "5 of 5",
    ]);
    shoes.update_property(&[gtk::accessible::Property::Label("Shoes")]);
    shoes.set_halign(gtk::Align::Start);

    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Send to DanceBrainz");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel);
    buttons.append(&save);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    column.append(&intro);
    column.append(&scope_row);
    column.append(&heading("Dances"));
    column.append(&present_list);
    column.append(&search_row);
    column.append(&suggestion_list);
    column.append(&planned_list);
    column.append(&heading("Meter"));
    column.append(&meter_row);
    column.append(&heading("Shoes"));
    column.append(&shoes);
    column.append(&status);
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&column)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.append(&scrolled);
    buttons.set_margin_bottom(12);
    buttons.set_margin_end(12);
    outer.append(&buttons);
    window.set_child(Some(&outer));

    let dialog = Rc::new(Dialog {
        window,
        runtime: Arc::clone(runtime),
        services,
        scope,
        every,
        targets: RefCell::default(),
        plan: RefCell::new(Plan::new()),
        present_list,
        planned_list,
        search: search.clone(),
        suggestions: RefCell::default(),
        suggestion_list,
        qualifier,
        meters: RefCell::default(),
        meter_choice,
        meter_names,
        sub_choice,
        sub_names,
        shoes,
        status,
        save,
        on_done,
    });

    let weak = Rc::downgrade(&dialog);
    dialog.every.connect_toggled(move |_| {
        if let Some(d) = weak.upgrade() {
            d.show_present();
        }
    });
    let weak = Rc::downgrade(&dialog);
    search.connect_search_changed(move |entry| {
        if let Some(d) = weak.upgrade() {
            d.suggest(entry.text().to_string());
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.suggestion_list.connect_row_activated(move |_, row| {
        if let Some(d) = weak.upgrade() {
            d.add_suggestion(row.index());
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.meter_choice.connect_selected_notify(move |_| {
        if let Some(d) = weak.upgrade() {
            d.meter_changed();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.sub_choice.connect_selected_notify(move |_| {
        if let Some(d) = weak.upgrade() {
            d.sub_changed();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.shoes.connect_selected_notify(move |choice| {
        if let Some(d) = weak.upgrade() {
            d.plan.borrow_mut().shoes = match choice.selected() {
                0 => Setting::Keep,
                1 => Setting::Clear,
                n => Setting::Set(u8::try_from(n - 1).unwrap_or(5).min(5)),
            };
            d.update_save();
        }
    });
    let weak = Rc::downgrade(&dialog);
    cancel.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.window.close();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.save.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.send();
        }
    });
    // The dialog lives until it is closed.
    let keep = RefCell::new(Some(Rc::clone(&dialog)));
    dialog.window.connect_close_request(move |_| {
        keep.borrow_mut().take();
        glib::Propagation::Proceed
    });
    dialog.window.present();
    dialog.load();
    dialog
}

impl Dialog {
    /// Runs `work` on the runtime and hands its result to `done` here.
    fn spawn<T: Send + 'static>(
        self: &Rc<Self>,
        work: impl std::future::Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        let handle = self.runtime.spawn(work);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if let Ok(value) = handle.await
                && let Some(me) = weak.upgrade()
            {
                done(&me, value);
            }
        });
    }

    fn load(self: &Rc<Self>) {
        self.status.set_text("Reading the tracks…");
        let db = Arc::clone(&self.services.db);
        let mut mbids = self.scope.all.clone();
        if !mbids.contains(&self.scope.one) {
            mbids.insert(0, self.scope.one.clone());
        }
        self.spawn(
            contribute::targets(db, mbids),
            |me, targets| match targets {
                Ok(targets) => {
                    *me.targets.borrow_mut() = targets;
                    me.status.set_text("");
                    me.show_present();
                }
                Err(error) => me.status.set_text(&error),
            },
        );
        self.spawn(
            contribute::meters(self.services.clone()),
            |me, meters| match meters {
                Ok(meters) => {
                    for value in meters.iter().filter(|v| v.parent_id.is_none()) {
                        me.meter_names.append(&value.name);
                    }
                    *me.meters.borrow_mut() = meters;
                }
                Err(error) => me.status.set_text(&error),
            },
        );
    }

    /// The tracks the changes apply to.
    fn chosen(&self) -> Vec<Target> {
        let targets = self.targets.borrow();
        if self.every.is_active() {
            targets
                .iter()
                .filter(|t| self.scope.all.contains(&t.mbid))
                .cloned()
                .collect()
        } else {
            targets
                .iter()
                .filter(|t| t.mbid == self.scope.one)
                .cloned()
                .collect()
        }
    }

    /// Lists the dances the chosen tracks have, each with a remove switch.
    fn show_present(self: &Rc<Self>) {
        self.present_list.remove_all();
        let chosen = self.chosen();
        let counts = dance_counts(&chosen);
        if counts.is_empty() {
            self.present_list.append(
                &gtk::Label::builder()
                    .label("No dance known")
                    .xalign(0.0)
                    .css_classes(["dim-label"])
                    .margin_start(6)
                    .build(),
            );
        }
        for (name, count) in counts {
            let label = if chosen.len() > 1 {
                format!("{name} (on {count} of {})", chosen.len())
            } else {
                name.clone()
            };
            let check = gtk::CheckButton::with_label(&format!("Remove {label}"));
            check.set_active(self.plan.borrow().remove.contains(&name));
            let weak = Rc::downgrade(self);
            check.connect_toggled(move |check| {
                if let Some(d) = weak.upgrade() {
                    let mut plan = d.plan.borrow_mut();
                    plan.remove.retain(|n| n != &name);
                    if check.is_active() {
                        plan.remove.push(name.clone());
                    }
                    drop(plan);
                    d.update_save();
                }
            });
            self.present_list.append(&check);
        }
        self.update_save();
    }

    fn suggest(self: &Rc<Self>, text: String) {
        if text.trim().is_empty() {
            self.suggestion_list.remove_all();
            self.suggestions.borrow_mut().clear();
            return;
        }
        let wanted = text.clone();
        self.spawn(
            contribute::suggest(self.services.clone(), text),
            move |me, found| {
                // A slower answer to an older text is dropped.
                if me.search.text() != wanted.as_str() {
                    return;
                }
                me.suggestion_list.remove_all();
                match found {
                    Ok(found) => {
                        for dance in &found {
                            let label = gtk::Label::builder()
                                .label(format!("Add {}", dance.path))
                                .xalign(0.0)
                                .margin_start(6)
                                .build();
                            me.suggestion_list.append(&label);
                        }
                        *me.suggestions.borrow_mut() = found;
                    }
                    Err(error) => me.status.set_text(&error),
                }
            },
        );
    }

    fn add_suggestion(self: &Rc<Self>, index: i32) {
        let Some(dance) = usize::try_from(index)
            .ok()
            .and_then(|i| self.suggestions.borrow().get(i).cloned())
        else {
            return;
        };
        let qualifier = QUALIFIERS
            .get(self.qualifier.selected() as usize)
            .map_or("both", |(key, _)| key)
            .to_owned();
        {
            let mut plan = self.plan.borrow_mut();
            plan.add.retain(|(d, _)| d.name != dance.name);
            plan.add.push((dance, qualifier));
        }
        self.show_planned();
    }

    fn show_planned(self: &Rc<Self>) {
        self.planned_list.remove_all();
        let adds = self.plan.borrow().add.clone();
        for (index, (dance, qualifier)) in adds.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let label = gtk::Label::builder()
                .label(format!("Will add {} ({qualifier})", dance.name))
                .xalign(0.0)
                .hexpand(true)
                .margin_start(6)
                .build();
            let undo = gtk::Button::from_icon_name("edit-delete-symbolic");
            undo.add_css_class("flat");
            undo.set_tooltip_text(Some("Do not add this dance"));
            undo.update_property(&[gtk::accessible::Property::Label("Do not add this dance")]);
            let weak = Rc::downgrade(self);
            undo.connect_clicked(move |_| {
                if let Some(d) = weak.upgrade() {
                    if index < d.plan.borrow().add.len() {
                        d.plan.borrow_mut().add.remove(index);
                    }
                    d.show_planned();
                }
            });
            row.append(&label);
            row.append(&undo);
            self.planned_list.append(&row);
        }
        self.update_save();
    }

    /// The meter chosen in the first drop-down, when one is.
    fn chosen_meter(&self) -> Option<TagValue> {
        let index = self.meter_choice.selected();
        let name = self.meter_names.string(index)?.to_string();
        (index >= 2)
            .then(|| {
                self.meters
                    .borrow()
                    .iter()
                    .find(|v| v.parent_id.is_none() && v.name == name)
                    .cloned()
            })
            .flatten()
    }

    fn meter_changed(self: &Rc<Self>) {
        let n = self.sub_names.n_items();
        self.sub_names
            .splice(1, n.saturating_sub(1), &[] as &[&str]);
        let meter = self.chosen_meter();
        if let Some(meter) = &meter {
            for sub in self
                .meters
                .borrow()
                .iter()
                .filter(|v| v.parent_id == Some(meter.id))
            {
                self.sub_names.append(&sub.name);
            }
        }
        self.sub_choice.set_selected(0);
        self.sub_choice.set_sensitive(self.sub_names.n_items() > 1);
        self.plan.borrow_mut().meter = match self.meter_choice.selected() {
            0 => Setting::Keep,
            1 => Setting::Clear,
            _ => meter.map_or(Setting::Keep, |meter| {
                Setting::Set(MeterChoice {
                    meter: meter.name,
                    sub: None,
                })
            }),
        };
        self.update_save();
    }

    fn sub_changed(self: &Rc<Self>) {
        let index = self.sub_choice.selected();
        let sub = (index >= 1)
            .then(|| self.sub_names.string(index).map(|s| s.to_string()))
            .flatten();
        if let Setting::Set(choice) = &mut self.plan.borrow_mut().meter {
            choice.sub = sub;
        }
        self.update_save();
    }

    fn update_save(&self) {
        let plan = self.plan.borrow();
        let something = !plan.add.is_empty()
            || !plan.remove.is_empty()
            || plan.meter != Setting::Keep
            || plan.shoes != Setting::Keep;
        self.save
            .set_sensitive(something && !self.targets.borrow().is_empty());
    }

    fn send(self: &Rc<Self>) {
        let edits = edits_for(&self.chosen(), &self.plan.borrow());
        if edits.is_empty() {
            return;
        }
        self.save.set_sensitive(false);
        self.status.set_text("Sending…");
        self.spawn(
            contribute::change(self.services.clone(), edits),
            |me, outcome| {
                let failed = outcome.is_err();
                if let Err(error) = &outcome {
                    me.status.set_text(error);
                    me.save.set_sensitive(true);
                }
                (me.on_done)(outcome);
                if !failed {
                    me.window.close();
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use studio_core::dancebrainz::{DanceTag, Qualifier};

    use super::*;

    const A: &str = "0f8b7c4e-2a7b-4a8e-9a42-3d7c5b1e9f10";
    const B: &str = "11111111-2222-4333-8444-555555555555";

    fn tag(kind: &str, name: &str, path: &[&str]) -> DanceTag {
        DanceTag {
            tag_id: name.bytes().map(i64::from).sum(),
            kind: kind.to_owned(),
            name: name.to_owned(),
            path: path.iter().map(|s| (*s).to_owned()).collect(),
            qualifier: Some(Qualifier::Both),
            updated_at: "2026-11-24".to_owned(),
        }
    }

    fn targets() -> Vec<Target> {
        vec![
            Target {
                mbid: Mbid::parse(A).unwrap(),
                cached: vec![
                    tag("dance", "Bourrée", &["Bourrée"]),
                    tag("dance", "Mazurka", &["Mazurka"]),
                    tag("meter", "3/4", &["3/4"]),
                ],
            },
            Target {
                mbid: Mbid::parse(B).unwrap(),
                cached: vec![tag("dance", "Mazurka", &["Mazurka"])],
            },
        ]
    }

    #[test]
    fn the_dances_of_a_list_are_counted() {
        assert_eq!(
            dance_counts(&targets()),
            [("Bourrée".to_owned(), 1), ("Mazurka".to_owned(), 2)]
        );
    }

    #[test]
    fn a_bulk_edit_queues_each_track_with_what_it_showed() {
        let plan = Plan {
            add: vec![(
                Suggestion {
                    tag_id: 3,
                    name: "Scottish".to_owned(),
                    path: "Scottish".to_owned(),
                },
                "learn".to_owned(),
            )],
            remove: vec!["Mazurka".to_owned()],
            meter: Setting::Set(MeterChoice {
                meter: "8/4".to_owned(),
                sub: Some("3+3+2".to_owned()),
            }),
            shoes: Setting::Keep,
        };
        let edits = edits_for(&targets(), &plan);
        assert_eq!(edits.len(), 6, "three changes on each of two tracks");
        assert_eq!(
            edits[0].1,
            Edit::RemoveDance {
                name: "Mazurka".to_owned(),
                shown: vec!["Bourrée".to_owned(), "Mazurka".to_owned()],
            }
        );
        assert_eq!(
            edits[2].1,
            Edit::SetMeter {
                to: plan_meter(),
                shown: Some(MeterChoice {
                    meter: "3/4".to_owned(),
                    sub: None
                }),
            }
        );
        assert_eq!(
            edits[5].1,
            Edit::SetMeter {
                to: plan_meter(),
                shown: None
            }
        );
    }

    fn pump_until(limit_ms: u64, done: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let end = std::time::Instant::now() + std::time::Duration::from_millis(limit_ms);
        while std::time::Instant::now() < end && !done() {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn save(widget: &gtk::Widget, path: &std::path::Path) {
        pump_until(400, || false);
        let paintable = gtk::WidgetPaintable::new(Some(widget));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(widget.width()),
            f64::from(widget.height()),
        );
        let Some(node) = snapshot.to_node() else {
            eprintln!("SHOT skipped, nothing drawn: {}", path.display());
            return;
        };
        let renderer = widget
            .native()
            .and_then(|n| n.renderer())
            .expect("renderer");
        renderer
            .render_texture(&node, None)
            .save_to_png(path)
            .expect("png");
        eprintln!("SHOT {}", path.display());
    }

    fn services(db: &Arc<studio_core::db::Database>) -> Services {
        Services {
            vault: Arc::new(studio_core::secrets::SecretVault::<
                crate::secret_service::SecretService,
            >::session_only("ui check")),
            db: Arc::clone(db),
        }
    }

    fn runtime() -> Arc<tokio::runtime::Runtime> {
        Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime"),
        )
    }

    /// Opens the dialog on two cached tracks, removes a dance and sets meter
    /// and shoes on both, and saves pictures to `SHOTS`. Unpaired, so the
    /// changes stay queued.
    #[test]
    #[ignore = "local: needs a display and SHOTS"]
    fn a_bulk_edit_is_queued_from_the_dialog() {
        use studio_core::dancebrainz::{RecordingDances, store};
        let shots = std::path::PathBuf::from(std::env::var("SHOTS").expect("SHOTS"));
        let base = shots.join("dances");
        let _ = std::fs::remove_dir_all(&base);
        let db = Arc::new(studio_core::db::Database::open(base.join("library.db")).expect("db"));
        let answers: Vec<RecordingDances> = targets()
            .into_iter()
            .map(|t| RecordingDances {
                mbid: t.mbid,
                tags: t.cached,
            })
            .collect();
        db.write(|tx| store(tx, &answers, 1)).expect("cache");
        let runtime = runtime();

        gtk::init().expect("gtk");
        let parent = gtk::ApplicationWindow::builder().build();
        parent.present();
        let done = Rc::new(RefCell::new(None));
        let seen = Rc::clone(&done);
        let dialog = open(
            &parent,
            &runtime,
            services(&db),
            Scope {
                one: Mbid::parse(A).unwrap(),
                all: vec![Mbid::parse(A).unwrap(), Mbid::parse(B).unwrap()],
                list: "Pingha".into(),
            },
            Rc::new(move |outcome| *seen.borrow_mut() = Some(outcome)),
        );
        pump_until(5000, || !dialog.targets.borrow().is_empty());
        pump_until(2000, || dialog.status.text().contains("Pair"));
        save(dialog.window.upcast_ref(), &shots.join("dances-1-one.png"));

        dialog.every.set_active(true);
        pump_until(200, || false);
        let mut remove = None;
        let mut child = dialog.present_list.first_child();
        while let Some(row) = child {
            if let Some(check) = row.first_child().and_downcast::<gtk::CheckButton>()
                && check
                    .label()
                    .is_some_and(|l| l.contains("Mazurka (on 2 of 2)"))
            {
                remove = Some(check);
            }
            child = row.next_sibling();
        }
        remove.expect("Mazurka is listed for both").set_active(true);
        dialog.meter_choice.set_selected(1);
        dialog.shoes.set_selected(4);
        assert!(dialog.save.is_sensitive());
        save(dialog.window.upcast_ref(), &shots.join("dances-2-bulk.png"));

        dialog.save.emit_clicked();
        pump_until(5000, || done.borrow().is_some());
        assert!(
            matches!(done.borrow().as_ref(), Some(Ok(None))),
            "queued, not paired"
        );
        let queued = studio_core::dance_edits::edits(&db.reader().unwrap()).unwrap();
        let kinds: Vec<String> = queued.iter().map(|e| e.edit.describe()).collect();
        assert_eq!(
            kinds,
            [
                "Remove dance Mazurka",
                "Clear the meter",
                "Set shoes to 3 of 5",
                "Remove dance Mazurka",
                "Clear the meter",
                "Set shoes to 3 of 5",
            ]
        );
    }

    fn plan_meter() -> Option<MeterChoice> {
        Some(MeterChoice {
            meter: "8/4".to_owned(),
            sub: Some("3+3+2".to_owned()),
        })
    }

    #[test]
    fn nothing_chosen_changes_nothing() {
        assert!(edits_for(&targets(), &Plan::new()).is_empty());
        let shoes = Plan {
            shoes: Setting::Clear,
            ..Plan::new()
        };
        assert_eq!(
            edits_for(&targets(), &shoes)
                .into_iter()
                .map(|(_, e)| e)
                .collect::<Vec<_>>(),
            [Edit::SetShoes { to: None }, Edit::SetShoes { to: None }]
        );
    }
}
