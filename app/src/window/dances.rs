//! The window's side of DanceBrainz contributions (#16): the dance facts in
//! the detail pane, the edit dialog, the account and queue dialog, and
//! sending the queue now and then.

use studio_core::dance_edits::Field;
use studio_core::dancebrainz::Mbid;

use super::*;
use crate::contribute;
use crate::{dance_dialog, dancebrainz_dialog};

/// How often the queue is sent while Studio runs, in seconds.
const SEND_EVERY_SECS: u32 = 5 * 60;

impl Window {
    /// The header button of the DanceBrainz dialog.
    pub(super) fn dancebrainz_button(self: &Rc<Self>) -> gtk::Button {
        let button = icon_button(
            "view-list-bullet-symbolic",
            "DanceBrainz: pairing and changes waiting to be sent",
        );
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.show_dancebrainz();
            }
        });
        button
    }

    /// Sends the queue now and every few minutes while Studio runs.
    pub(super) fn start_dance_edits(self: &Rc<Self>) {
        self.send_dance_edits();
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(SEND_EVERY_SECS, move || {
            let Some(me) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            me.send_dance_edits();
            glib::ControlFlow::Continue
        });
    }

    /// Tries to send the queue, quietly unless something happened.
    fn send_dance_edits(self: &Rc<Self>) {
        let Some(services) = self.services.borrow().clone() else {
            return;
        };
        self.spawn(contribute::send(services), |me, sent| match sent {
            Ok(Some(report)) => {
                let quiet = report.sent == 0
                    && report.held == 0
                    && report.conflicts == 0
                    && report.failed == 0;
                if !quiet {
                    me.status.set_text(&contribute::report_text(&report));
                    me.refresh_dance_facts();
                }
            }
            Ok(None) => {}
            Err(error) => tracing::info!(%error, "the DanceBrainz queue was not sent"),
        });
    }

    fn show_dancebrainz(self: &Rc<Self>) {
        let Some(services) = self.services.borrow().clone() else {
            self.banner
                .show("Studio is still starting. Try again in a moment.");
            return;
        };
        let weak = Rc::downgrade(self);
        dancebrainz_dialog::present(
            &self.window,
            &self.runtime,
            services,
            Rc::new(move |text: &str| {
                if let Some(me) = weak.upgrade() {
                    if !text.is_empty() {
                        me.status.set_text(text);
                    }
                    me.refresh_dance_facts();
                }
            }),
        );
    }

    /// "Edit dances…" for the track shown and the other tracks of the list.
    pub(super) fn edit_dances(self: &Rc<Self>) {
        let Some(services) = self.services.borrow().clone() else {
            self.banner
                .show("Studio is still starting. Try again in a moment.");
            return;
        };
        let Some(one) = self
            .detail
            .song
            .borrow()
            .as_ref()
            .and_then(|s| s.mbid.as_deref())
            .and_then(|m| Mbid::parse(m).ok())
        else {
            return;
        };
        let mut all = Vec::new();
        for item in self.tracks.store.iter::<BoxedAnyObject>().flatten() {
            let song = item.borrow::<Song>();
            if let Some(mbid) = song.mbid.as_deref().and_then(|m| Mbid::parse(m).ok())
                && !all.contains(&mbid)
            {
                all.push(mbid);
            }
        }
        if !all.contains(&one) {
            all.insert(0, one.clone());
        }
        let list = self
            .tracks
            .shown
            .borrow()
            .as_ref()
            .map(|(title, _)| title.clone())
            .unwrap_or_default();
        let weak = Rc::downgrade(self);
        dance_dialog::present(
            &self.window,
            &self.runtime,
            services,
            dance_dialog::Scope { one, all, list },
            Rc::new(move |outcome| {
                let Some(me) = weak.upgrade() else { return };
                match outcome {
                    Ok(Some(report)) => me.status.set_text(&contribute::report_text(&report)),
                    Ok(None) => me
                        .status
                        .set_text("The changes wait until Studio is paired with DanceBrainz."),
                    Err(error) => me.status.set_text(&error),
                }
                me.refresh_dance_facts();
            }),
        );
    }

    /// Reads the dance facts of the shown track again.
    fn refresh_dance_facts(self: &Rc<Self>) {
        let song = self.detail.song.borrow().clone();
        if let Some(song) = song {
            self.show_dance_facts(&song);
        }
    }

    /// Fills the dance, meter and shoes rows of the detail pane.
    pub(super) fn show_dance_facts(self: &Rc<Self>, song: &Song) {
        let d = &self.detail;
        let mbid = song.mbid.as_deref().and_then(|m| Mbid::parse(m).ok());
        d.edit_dances.set_sensitive(mbid.is_some());
        let (Some(mbid), Some(services)) = (mbid, self.services.borrow().clone()) else {
            d.dances
                .set_text("No MusicBrainz ID, so DanceBrainz cannot tell");
            d.meter.set_text("");
            d.shoes.set_text("");
            return;
        };
        for label in [&d.dances, &d.meter, &d.shoes] {
            label.set_text("…");
            label.set_tooltip_text(None);
        }
        let expected = song.id.clone();
        self.spawn(contribute::view(services, mbid), move |me, view| {
            if !me.detail_is(&expected) {
                return;
            }
            let d = &me.detail;
            let view = match view {
                Ok(view) => view,
                Err(error) => {
                    d.dances.set_text(&error);
                    return;
                }
            };
            let shown = &view.shown;
            let rows = [
                (&d.dances, Field::Dance, contribute::dances_text(shown)),
                (
                    &d.meter,
                    Field::Meter,
                    contribute::meter_text(shown.meter.as_ref()),
                ),
                (&d.shoes, Field::Shoes, contribute::shoes_text(&view)),
            ];
            for (label, field, text) in rows {
                let (text, why) = contribute::with_state(shown, field, text);
                label.set_text(&text);
                label.set_tooltip_text(why.as_deref());
            }
        });
    }
}
