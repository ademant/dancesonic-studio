//! Handing the play queue to another device and taking one over (#18).
//!
//! "Hand off" saves the queue, the song that plays and its position on the
//! server and pauses here; the phone (or Studio elsewhere) goes on from
//! there. "Continue" reads the queue another device saved and plays it from
//! where that device stopped.

use super::*;

impl Window {
    /// The header's hand-off menu.
    pub(super) fn handoff_menu(self: &Rc<Self>) -> gtk::MenuButton {
        let hand_off = gtk::Button::with_label("Hand off to another device");
        hand_off.set_tooltip_text(Some(
            "Save the queue and the position on the server, then pause here",
        ));
        let take_over = gtk::Button::with_label("Continue from another device");
        take_over.set_tooltip_text(Some(
            "Play the queue another device saved, from where it stopped",
        ));
        for button in [&hand_off, &take_over] {
            button.add_css_class("flat");
        }
        let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
        column.append(&hand_off);
        column.append(&take_over);
        let popover = gtk::Popover::builder().child(&column).build();
        let menu = gtk::MenuButton::builder()
            .icon_name("document-send-symbolic")
            .tooltip_text("Hand the queue to another device")
            .popover(&popover)
            .build();
        menu.update_property(&[gtk::accessible::Property::Label(
            "Hand the queue to another device",
        )]);
        let weak = Rc::downgrade(self);
        let shown = popover.clone();
        hand_off.connect_clicked(move |_| {
            shown.popdown();
            if let Some(me) = weak.upgrade() {
                me.hand_off();
            }
        });
        let weak = Rc::downgrade(self);
        let shown = popover;
        take_over.connect_clicked(move |_| {
            shown.popdown();
            if let Some(me) = weak.upgrade() {
                me.take_over();
            }
        });
        menu
    }

    /// Saves the queue on the server and pauses.
    pub(super) fn hand_off(self: &Rc<Self>) {
        let Some(session) = self.session.borrow().clone() else {
            self.status
                .set_text("Connect to a server to hand off the queue.");
            return;
        };
        let Some(current) = self.now_playing.get() else {
            self.status
                .set_text("Nothing plays, so there is no queue to hand off.");
            return;
        };
        let queue: Vec<String> = self.queue.borrow().iter().map(|s| s.id.clone()).collect();
        let position = self
            .player
            .borrow()
            .as_ref()
            .and_then(Player::position_ms)
            .map_or(0, |(position, _)| position);
        self.status.set_text("Saving the queue on the server…");
        self.spawn(
            async move { session.hand_off(queue, current, position).await },
            |me, saved| match saved {
                Ok(count) => {
                    if me.playing.get() {
                        me.toggle_play();
                    }
                    me.status.set_text(&handed_off_text(count));
                }
                Err(error) => me
                    .status
                    .set_text(&format!("The queue was not saved: {error}")),
            },
        );
    }

    /// Plays the queue another device saved.
    pub(super) fn take_over(self: &Rc<Self>) {
        let Some(session) = self.session.borrow().clone() else {
            self.status
                .set_text("Connect to a server to continue a queue.");
            return;
        };
        self.status.set_text("Reading the saved queue…");
        self.spawn(
            async move { session.saved_queue().await },
            |me, saved| match saved {
                Ok(Some((songs, start, position))) => {
                    let count = songs.len();
                    match me.load_songs_at(songs, start, position) {
                        Ok(()) => {
                            me.status.set_text(&format!(
                                "Continuing a queue of {count} {} at {}.",
                                if count == 1 { "song" } else { "songs" },
                                clock(position)
                            ));
                            me.publish_remote();
                        }
                        Err(_) => me
                            .status
                            .set_text("None of the saved songs can be played here."),
                    }
                }
                Ok(None) => me
                    .status
                    .set_text("No queue is saved on the server for this account."),
                Err(error) => me
                    .status
                    .set_text(&format!("The saved queue could not be read: {error}")),
            },
        );
    }
}

/// What the status line says after a hand-off.
fn handed_off_text(count: usize) -> String {
    format!(
        "Saved {count} {} on the server. Continue on the other device.",
        if count == 1 { "song" } else { "songs" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hand_off_says_how_much_was_saved() {
        assert_eq!(
            handed_off_text(1),
            "Saved 1 song on the server. Continue on the other device."
        );
        assert!(handed_off_text(12).starts_with("Saved 12 songs"));
    }
}
